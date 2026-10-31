use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use taproot_core::db::{Database, RlsContext, SqlxBackend};
use taproot_core::planner::{
    execute_batch_lookup, execute_plan, execute_plan_count, FilterExpr, OrderByExpr, OrderDirection,
    PlanNode,
};
use taproot_core::registry::types::SchemaOptions;
use taproot_core::subscription::{ChangeBroadcaster, ChangeNotification, ChangeOp};
use futures_util::StreamExt;
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

    build_schema(&introspected, &db_arc, &schema_opts, &[], None).expect("schema build failed")
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

    let query = r#"{ user(rowId: "00000000-0000-0000-0000-000000000001") { name } }"#;
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
async fn connection_backward_pagination() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    // last: 1 over [Alice, Bob] ordered by id asc -> Bob, with a previous page
    let result = gql_schema
        .execute("{ users(orderBy: [ROW_ID_ASC], last: 1) { nodes { name } totalCount pageInfo { hasNextPage hasPreviousPage } } }")
        .await;
    assert!(result.errors.is_empty(), "last-page errors: {:?}", result.errors);

    let data = result.data.into_json().expect("serialize");
    let conn = &data["users"];
    assert_eq!(conn["totalCount"], 2);
    let nodes = conn["nodes"].as_array().expect("nodes");
    assert_eq!(nodes.len(), 1, "expected 1 node");
    assert_eq!(nodes[0]["name"], "Bob");
    assert_eq!(conn["pageInfo"]["hasPreviousPage"], true);
    assert_eq!(conn["pageInfo"]["hasNextPage"], false);

    // Grab Bob's cursor, then page backward with before + last
    let edges_result = gql_schema
        .execute("{ users(orderBy: [ROW_ID_ASC], first: 2) { edges { cursor node { name } } } }")
        .await;
    let edges_data = edges_result.data.into_json().expect("serialize");
    let edges = edges_data["users"]["edges"].as_array().expect("edges");
    assert_eq!(edges[1]["node"]["name"], "Bob");
    let bob_cursor = edges[1]["cursor"].as_str().expect("cursor").to_owned();

    let before_query = format!(
        r#"{{ users(orderBy: [ROW_ID_ASC], last: 1, before: "{bob_cursor}") {{ nodes {{ name }} }} }}"#
    );
    let before_result = gql_schema.execute(&before_query).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(before_result.errors.is_empty(), "before errors: {:?}", before_result.errors);
    let before_data = before_result.data.into_json().expect("serialize");
    let before_nodes = before_data["users"]["nodes"].as_array().expect("nodes");
    assert_eq!(before_nodes.len(), 1, "expected 1 node before Bob");
    assert_eq!(before_nodes[0]["name"], "Alice");
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
    let query = r#"{ users(filter: { name: { equalTo: "Alice" } }) { nodes { name email } totalCount } }"#;
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
                rowId: "00000000-0000-0000-0000-000000000001",
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
                rowId: "20000000-0000-0000-0000-000000000003"
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
                rowId: "10000000-0000-0000-0000-000000000002"
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
                rowId: "20000000-0000-0000-0000-000000000001"
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
                rowId: "00000000-0000-0000-0000-000000000002"
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
    let verify = r#"{ user(rowId: "00000000-0000-0000-0000-000000000002") { name } }"#;
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
        r#"{ user(rowId: "00000000-0000-0000-0000-000000000001") { name postsByAuthorId { nodes { title } totalCount } } }"#;
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
        build_schema(&introspected, &db_arc, &SchemaOptions::default(), &plugins, None).unwrap();
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
            value: serde_json::json!("Alice"), cast: None,
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
                value: serde_json::json!("Alice"), cast: None,
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
                value: serde_json::json!("Alice"), cast: None,
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
        build_schema(&introspected, &db_arc, &SchemaOptions::default(), &plugins, None).unwrap();

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
        .execute(r#"{ user(rowId: "00000000-0000-0000-0000-000000000002") { name } }"#)
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
            value: serde_json::json!(true), cast: None,
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
        None,
    )
    .expect("schema build should succeed");

    // Query Alice's posts — plugin should filter to only published
    let result = gql_schema
        .execute(
            r#"{ user(rowId: "00000000-0000-0000-0000-000000000001") { name postsByAuthorId { nodes { title published } totalCount } } }"#,
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
        build_schema(&introspected, &db_arc, &options, &[], None).expect("schema build should succeed");

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
        build_schema(&introspected, &db_arc, &options, &[], None).expect("schema build should succeed");

    // Resolve Alice by her entity key
    let query = r#"
        {
            _entities(representations: [
                { __typename: "User", rowId: "00000000-0000-0000-0000-000000000001" }
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
        build_schema(&introspected, &db_arc, &options, &[], None).expect("schema build should succeed");

    // Resolve both Alice and Bob in a single _entities call
    let query = r#"
        {
            _entities(representations: [
                { __typename: "User", rowId: "00000000-0000-0000-0000-000000000001" },
                { __typename: "User", rowId: "00000000-0000-0000-0000-000000000002" }
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
        build_schema(&introspected, &db_arc, &options, &plugins, None).expect("schema build");

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

#[tokio::test]
async fn rls_filters_rows_by_context() {
    let Some(db) = connect().await else {
        return;
    };
    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    // Create a test role for RLS
    let role_name = format!("{schema_name}_role");
    let rls_ddl = [
        // Enable RLS on users table
        format!("ALTER TABLE \"{schema_name}\".users ENABLE ROW LEVEL SECURITY"),
        format!("ALTER TABLE \"{schema_name}\".users FORCE ROW LEVEL SECURITY"),
        // Create policy: user can only see their own row based on JWT sub claim
        format!(
            "CREATE POLICY users_own_row ON \"{schema_name}\".users \
             FOR SELECT USING (id::text = current_setting('request.jwt.claims', true)::jsonb->>'sub')"
        ),
        // Create role and grant access
        format!("CREATE ROLE \"{role_name}\" NOLOGIN"),
        format!("GRANT USAGE ON SCHEMA \"{schema_name}\" TO \"{role_name}\""),
        format!("GRANT SELECT ON ALL TABLES IN SCHEMA \"{schema_name}\" TO \"{role_name}\""),
    ];

    for stmt in &rls_ddl {
        db.query_raw(stmt)
            .await
            .unwrap_or_else(|e| panic!("RLS DDL failed: {stmt}: {e}"));
    }

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
    let gql_schema =
        build_schema(&introspected, &db_arc, &SchemaOptions::default(), &[], None).expect("schema build");

    // Query as Alice — should only see Alice's row
    let rls_ctx = RlsContext {
        role: Some(role_name.clone()),
        claims_json: Some(r#"{"sub":"00000000-0000-0000-0000-000000000001"}"#.into()),
        claims_setting: "request.jwt.claims".into(),
    };

    let request =
        async_graphql::Request::new(r"{ users(first: 10) { edges { node { name } } } }")
            .data(rls_ctx);

    let result = gql_schema.execute(request).await;
    assert!(result.errors.is_empty(), "errors: {:?}", result.errors);

    let data = result.data.into_json().expect("serialize");
    let edges = data["users"]["edges"].as_array().expect("array");

    // RLS should filter to only Alice
    assert_eq!(
        edges.len(),
        1,
        "RLS should filter to one user, got {}",
        edges.len()
    );
    assert_eq!(edges[0]["node"]["name"], "Alice");

    // Query as Bob — should only see Bob
    let rls_ctx_bob = RlsContext {
        role: Some(role_name.clone()),
        claims_json: Some(r#"{"sub":"00000000-0000-0000-0000-000000000002"}"#.into()),
        claims_setting: "request.jwt.claims".into(),
    };

    let request_bob =
        async_graphql::Request::new(r"{ users(first: 10) { edges { node { name } } } }")
            .data(rls_ctx_bob);

    let result_bob = gql_schema.execute(request_bob).await;
    assert!(
        result_bob.errors.is_empty(),
        "errors: {:?}",
        result_bob.errors
    );

    let data_bob = result_bob.data.into_json().expect("serialize");
    let edges_bob = data_bob["users"]["edges"].as_array().expect("array");
    assert_eq!(
        edges_bob.len(),
        1,
        "RLS should filter to one user for Bob"
    );
    assert_eq!(edges_bob[0]["node"]["name"], "Bob");

    // Cleanup: drop the role
    let _ = db
        .query_raw(&format!("DROP ROLE IF EXISTS \"{role_name}\""))
        .await;
    teardown_test_schema(&db, &schema_name).await;
}

#[tokio::test]
async fn subscription_fields_in_schema() {
    let Some(db) = connect().await else { return };
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
    let broadcaster = ChangeBroadcaster::new(64);

    let options = SchemaOptions {
        subscriptions: true,
        ..Default::default()
    };

    let gql_schema = build_schema(&introspected, &db_arc, &options, &[], Some(&broadcaster))
        .expect("schema build");

    let sdl = gql_schema.sdl();
    assert!(
        sdl.contains("type Subscription"),
        "SDL should have Subscription type: {sdl}"
    );
    assert!(
        sdl.contains("onUserChanged"),
        "SDL should have onUserChanged: {sdl}"
    );
    assert!(
        sdl.contains("onPostChanged"),
        "SDL should have onPostChanged: {sdl}"
    );
    assert!(
        sdl.contains("onCommentChanged"),
        "SDL should have onCommentChanged: {sdl}"
    );

    teardown_test_schema(&db, &schema_name).await;
}

#[tokio::test]
async fn subscription_receives_change_notification() {
    let Some(db) = connect().await else { return };
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
    let broadcaster = ChangeBroadcaster::new(64);

    let options = SchemaOptions {
        subscriptions: true,
        ..Default::default()
    };

    let gql_schema = build_schema(&introspected, &db_arc, &options, &[], Some(&broadcaster))
        .expect("schema build");

    // Start a subscription stream
    let mut stream =
        gql_schema.execute_stream("subscription { onUserChanged { name email } }");

    // Send the notification from a spawned task after a short delay,
    // giving the stream poll time to register the broadcast receiver
    let bc = broadcaster.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        bc.send(ChangeNotification {
            table: "users".into(),
            op: ChangeOp::Update,
            id: "00000000-0000-0000-0000-000000000001".into(),
        });
    });

    // The stream should yield Alice's data
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .expect("timeout waiting for subscription")
        .expect("stream ended");

    assert!(response.errors.is_empty(), "errors: {:?}", response.errors);
    let data = response.data.into_json().expect("serialize");
    assert_eq!(data["onUserChanged"]["name"], "Alice");
    assert_eq!(data["onUserChanged"]["email"], "alice@example.com");

    teardown_test_schema(&db, &schema_name).await;
}

#[cfg(feature = "lattice")]
#[tokio::test]
async fn taproot_as_lattice_subgraph() {
    use taproot_core::embed::{TaprootSubgraph, TaprootSubgraphOptions};
    use taproot_core::lattice::{SubgraphExecutor, SubgraphRequest, TaprootExecutor};

    let Some(db) = connect().await else {
        return;
    };
    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let db_arc: Arc<dyn Database> = Arc::new(db.clone());

    let subgraph = TaprootSubgraph::build(
        Arc::clone(&db_arc),
        TaprootSubgraphOptions {
            schemas: vec![schema_name.clone()],
            ..Default::default()
        },
    )
    .await
    .expect("subgraph build");

    // Verify federation SDL
    let sdl = subgraph.sdl();
    assert!(sdl.contains("@key"), "SDL should contain @key: {sdl}");

    // Create executor
    let executor = TaprootExecutor::new(subgraph);

    // Execute via SubgraphExecutor trait
    let response = executor
        .execute_graphql(SubgraphRequest {
            url: String::new(),
            query: "{ users(first: 10) { edges { node { name } } } }".into(),
            operation_name: None,
            variables: None,
            headers: vec![],
        })
        .await
        .expect("execution");

    let data = response.data.expect("should have data");
    let edges = data["users"]["edges"].as_array().expect("array");
    assert_eq!(edges.len(), 2);

    teardown_test_schema(&db, &schema_name).await;
}

#[tokio::test]
async fn query_depth_limit_enforced() {
    let Some(db) = connect().await else {
        return;
    };
    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let db_arc: Arc<dyn Database> = Arc::new(db.clone());

    let introspected = introspect(
        &db,
        &IntrospectOptions {
            schemas: vec![schema_name.clone()],
            ..Default::default()
        },
    )
    .await
    .expect("introspection");

    let options = SchemaOptions {
        max_depth: 3,
        ..Default::default()
    };

    let gql_schema = build_schema(&introspected, &db_arc, &options, &[], None)
        .expect("schema build");

    // This query is 5 levels deep -- should be rejected
    let result = gql_schema
        .execute(r"{ users(first: 1) { edges { node { postsByAuthorId(first: 1) { edges { node { title } } } } } } }")
        .await;

    assert!(!result.errors.is_empty(), "deep query should be rejected");
    assert!(
        result.errors[0].message.to_lowercase().contains("too deep"),
        "error should mention depth: {:?}",
        result.errors
    );

    teardown_test_schema(&db, &schema_name).await;
}

// --- Composite primary key tests ---

/// Set up a schema with a composite PK join table for testing.
async fn setup_composite_pk_schema(db: &dyn Database, schema: &str) {
    let ddl = [
        &format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\""),
        &format!(
            "CREATE TABLE \"{schema}\".users (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                name TEXT NOT NULL
            )"
        ),
        &format!(
            "CREATE TABLE \"{schema}\".roles (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                name TEXT NOT NULL
            )"
        ),
        &format!(
            "CREATE TABLE \"{schema}\".user_roles (
                user_id UUID NOT NULL REFERENCES \"{schema}\".users(id),
                role_id UUID NOT NULL REFERENCES \"{schema}\".roles(id),
                granted_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                PRIMARY KEY (user_id, role_id)
            )"
        ),
    ];

    for stmt in &ddl {
        db.query_raw(stmt).await.expect("DDL statement failed");
    }

    let seed = [
        &format!(
            "INSERT INTO \"{schema}\".users (id, name) VALUES \
             ('00000000-0000-0000-0000-000000000001', 'Alice'), \
             ('00000000-0000-0000-0000-000000000002', 'Bob')"
        ) as &str,
        &format!(
            "INSERT INTO \"{schema}\".roles (id, name) VALUES \
             ('a0000000-0000-0000-0000-000000000001', 'admin'), \
             ('a0000000-0000-0000-0000-000000000002', 'editor')"
        ),
        &format!(
            "INSERT INTO \"{schema}\".user_roles (user_id, role_id) VALUES \
             ('00000000-0000-0000-0000-000000000001', 'a0000000-0000-0000-0000-000000000001')"
        ),
    ];

    for stmt in &seed {
        db.query_raw(stmt).await.expect("Seed statement failed");
    }
}

async fn build_composite_test_schema(
    db: &SqlxBackend,
    schema_name: &str,
) -> async_graphql::dynamic::Schema {
    let options = IntrospectOptions {
        schemas: vec![schema_name.to_string()],
        exclude_tables: Vec::new(),
        smart_comments: true,
    };

    let db_arc: Arc<dyn Database> = Arc::new(db.clone());
    let introspected = introspect(db_arc.as_ref(), &options)
        .await
        .expect("introspection failed");

    build_schema(
        &introspected,
        &db_arc,
        &SchemaOptions::default(),
        &[],
        None,
    )
    .expect("schema build failed")
}

#[tokio::test]
async fn composite_pk_create_mutation() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_composite_pk_schema(&db, &schema_name).await;
    let gql_schema = build_composite_test_schema(&db, &schema_name).await;

    let mutation = r#"
        mutation {
            createUserRole(input: {
                userId: "00000000-0000-0000-0000-000000000002",
                roleId: "a0000000-0000-0000-0000-000000000002"
            }) {
                userRole {
                    userId
                    roleId
                }
            }
        }
    "#;

    let result = gql_schema.execute(mutation).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "composite PK create mutation errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    let payload = &data["createUserRole"];
    assert_eq!(payload["userRole"]["userId"], "00000000-0000-0000-0000-000000000002");
    assert_eq!(payload["userRole"]["roleId"], "a0000000-0000-0000-0000-000000000002");
}

#[tokio::test]
async fn composite_pk_update_mutation() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_composite_pk_schema(&db, &schema_name).await;
    let gql_schema = build_composite_test_schema(&db, &schema_name).await;

    let mutation = r#"
        mutation {
            updateUserRole(input: {
                userId: "00000000-0000-0000-0000-000000000001",
                roleId: "a0000000-0000-0000-0000-000000000001",
                patch: {
                    grantedAt: "2025-01-01T00:00:00Z"
                }
            }) {
                userRole {
                    userId
                    roleId
                    grantedAt
                }
            }
        }
    "#;

    let result = gql_schema.execute(mutation).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "composite PK update mutation errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    let payload = &data["updateUserRole"];
    assert_eq!(payload["userRole"]["userId"], "00000000-0000-0000-0000-000000000001");
    assert_eq!(payload["userRole"]["roleId"], "a0000000-0000-0000-0000-000000000001");
}

#[tokio::test]
async fn composite_pk_delete_mutation() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_composite_pk_schema(&db, &schema_name).await;
    let gql_schema = build_composite_test_schema(&db, &schema_name).await;

    let mutation = r#"
        mutation {
            deleteUserRole(input: {
                userId: "00000000-0000-0000-0000-000000000001",
                roleId: "a0000000-0000-0000-0000-000000000001"
            }) {
                userRole {
                    userId
                    roleId
                }
            }
        }
    "#;

    let result = gql_schema.execute(mutation).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "composite PK delete mutation errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    assert!(!data["deleteUserRole"]["userRole"].is_null(), "deleted row should be returned");
}

#[tokio::test]
async fn composite_pk_singular_query() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_composite_pk_schema(&db, &schema_name).await;
    let gql_schema = build_composite_test_schema(&db, &schema_name).await;

    // Composite PK singular query: userRoleByUserIdAndRoleId
    let query = r#"{
        userRoleByUserIdAndRoleId(
            userId: "00000000-0000-0000-0000-000000000001",
            roleId: "a0000000-0000-0000-0000-000000000001"
        ) {
            userId
            roleId
        }
    }"#;

    let result = gql_schema.execute(query).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "composite PK singular query errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    assert_eq!(
        data["userRoleByUserIdAndRoleId"]["userId"],
        "00000000-0000-0000-0000-000000000001"
    );
}

#[tokio::test]
async fn aggregates_on_connection() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();

    db.query_raw(&format!("CREATE SCHEMA IF NOT EXISTS \"{schema_name}\""))
        .await
        .expect("create schema");
    db.query_raw(&format!(
        "CREATE TABLE \"{schema_name}\".orders (
            id SERIAL PRIMARY KEY,
            amount NUMERIC NOT NULL,
            quantity INTEGER NOT NULL,
            label TEXT NOT NULL
        )"
    ))
    .await
    .expect("create orders table");
    db.query_raw(&format!(
        "INSERT INTO \"{schema_name}\".orders (amount, quantity, label) VALUES \
         (10.5, 2, 'alpha'), \
         (20.0, 5, 'beta'), \
         (30.5, 3, 'gamma')"
    ))
    .await
    .expect("seed orders");

    let gql_schema = build_test_schema(&db, &schema_name).await;

    let query = r"{
        orders(first: 10) {
            aggregates {
                count
                sum { amount quantity }
                avg { amount }
                min { amount quantity label }
                max { amount quantity label }
            }
            edges { node { amount quantity } }
        }
    }";

    let result = gql_schema.execute(query).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "aggregates query errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    let agg = &data["orders"]["aggregates"];

    assert_eq!(agg["count"], 3);

    // Sum: 10.5 + 20.0 + 30.5 = 61.0; 2 + 5 + 3 = 10
    let sum_amount: f64 = agg["sum"]["amount"].as_f64().expect("sum.amount");
    assert!((sum_amount - 61.0).abs() < 0.01, "sum.amount = {sum_amount}");
    let sum_qty: f64 = agg["sum"]["quantity"].as_f64().expect("sum.quantity");
    assert!((sum_qty - 10.0).abs() < 0.01, "sum.quantity = {sum_qty}");

    // Avg amount: 61.0 / 3 ≈ 20.333
    let avg_amount: f64 = agg["avg"]["amount"].as_f64().expect("avg.amount");
    assert!(
        (avg_amount - 20.333).abs() < 0.01,
        "avg.amount = {avg_amount}"
    );

    // Min
    let min_amount: f64 = agg["min"]["amount"].as_f64().expect("min.amount");
    assert!((min_amount - 10.5).abs() < 0.01, "min.amount = {min_amount}");
    assert_eq!(agg["min"]["label"], "alpha");

    // Max
    let max_amount: f64 = agg["max"]["amount"].as_f64().expect("max.amount");
    assert!((max_amount - 30.5).abs() < 0.01, "max.amount = {max_amount}");
    assert_eq!(agg["max"]["label"], "gamma");
}

#[tokio::test]
async fn upsert_with_on_conflict() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    // Query the actual constraint name from pg_constraint
    let constraint_rows = db
        .query_raw(&format!(
            "SELECT con.conname FROM pg_constraint con \
             JOIN pg_class rel ON rel.oid = con.conrelid \
             JOIN pg_namespace nsp ON nsp.oid = rel.relnamespace \
             WHERE nsp.nspname = '{schema_name}' \
             AND rel.relname = 'users' \
             AND con.contype = 'u'"
        ))
        .await
        .expect("failed to query constraint name");

    let constraint_name = constraint_rows[0]["conname"]
        .as_str()
        .expect("constraint name must be a string");

    let gql_schema = build_test_schema(&db, &schema_name).await;

    // Upsert: alice@example.com already exists from seed data, so this
    // should update the name instead of inserting a duplicate
    let mutation = format!(
        r#"
        mutation {{
            createUser(input: {{
                name: "Updated Alice",
                email: "alice@example.com",
                role: admin,
                onConflict: {{
                    constraint: "{constraint_name}",
                    updateColumns: ["name"]
                }}
            }}) {{
                user {{
                    name
                    email
                    role
                }}
            }}
        }}
    "#
    );

    let result = gql_schema.execute(&mutation).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "upsert mutation errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    let payload = &data["createUser"];
    assert_eq!(payload["user"]["name"], "Updated Alice");
    assert_eq!(payload["user"]["email"], "alice@example.com");
    // Role should remain admin since only name was in updateColumns
    assert_eq!(payload["user"]["role"], "admin");
}

#[tokio::test]
async fn bulk_create_mutation() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    let mutation = r#"
        mutation {
            createUsers(input: {
                objects: [
                    { name: "Charlie", email: "charlie@example.com", role: viewer },
                    { name: "Dana", email: "dana@example.com", role: editor }
                ]
            }) {
                affectedRows
                users {
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
        "bulk create mutation errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    let payload = &data["createUsers"];
    assert_eq!(payload["affectedRows"], 2);

    let users = payload["users"].as_array().expect("users should be an array");
    assert_eq!(users.len(), 2);
    assert_eq!(users[0]["name"], "Charlie");
    assert_eq!(users[0]["email"], "charlie@example.com");
    assert_eq!(users[1]["name"], "Dana");
    assert_eq!(users[1]["email"], "dana@example.com");
}

#[tokio::test]
async fn bulk_update_mutation() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    // Update all viewers to editors (Alice is admin, Bob is editor — neither match)
    // First add a viewer
    let setup = r#"
        mutation {
            createUser(input: {
                name: "Charlie",
                email: "charlie@example.com",
                role: viewer
            }) {
                user { name }
            }
        }
    "#;
    let setup_result = gql_schema.execute(setup).await;
    assert!(setup_result.errors.is_empty(), "setup errors: {:?}", setup_result.errors);

    let mutation = r#"
        mutation {
            updateUsers(input: {
                where: { role: { equalTo: "viewer" } },
                patch: { role: editor }
            }) {
                affectedRows
                users {
                    name
                    role
                }
            }
        }
    "#;

    let result = gql_schema.execute(mutation).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "bulk update mutation errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    let payload = &data["updateUsers"];
    assert_eq!(payload["affectedRows"], 1);

    let users = payload["users"].as_array().expect("users should be an array");
    assert_eq!(users.len(), 1);
    assert_eq!(users[0]["name"], "Charlie");
    assert_eq!(users[0]["role"], "editor");
}

#[tokio::test]
async fn bulk_delete_mutation() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    // Delete all comments by Bob (author_id = 000...002)
    let mutation = r#"
        mutation {
            deleteComments(input: {
                where: { authorId: { equalTo: "00000000-0000-0000-0000-000000000002" } }
            }) {
                affectedRows
                comments {
                    body
                }
            }
        }
    "#;

    let result = gql_schema.execute(mutation).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "bulk delete mutation errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    let payload = &data["deleteComments"];
    assert_eq!(payload["affectedRows"], 1);

    let comments = payload["comments"].as_array().expect("comments should be an array");
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0]["body"], "Great post!");
}

#[tokio::test]
async fn computed_field_from_sql_function() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    // Create a stable SQL function whose first arg is a users row type
    let create_fn = format!(
        "CREATE FUNCTION \"{schema_name}\".user_display_name(u \"{schema_name}\".users) \
         RETURNS text AS $$ \
           SELECT u.name || ' <' || u.email || '>' \
         $$ LANGUAGE sql STABLE"
    );
    db.query_raw(&create_fn)
        .await
        .expect("create function failed");

    let gql_schema = build_test_schema(&db, &schema_name).await;

    // Query the computed field via GraphQL
    let query = r#"
        query {
            user(rowId: "00000000-0000-0000-0000-000000000001") {
                name
                userDisplayName
            }
        }
    "#;

    let result = gql_schema.execute(query).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "computed field query errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    let user = &data["user"];
    assert_eq!(user["name"], "Alice");
    assert_eq!(user["userDisplayName"], "Alice <alice@example.com>");
}

#[tokio::test]
async fn computed_field_volatile_excluded() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    // Create a volatile function — should NOT appear as a computed field
    let create_fn = format!(
        "CREATE FUNCTION \"{schema_name}\".user_random_tag(u \"{schema_name}\".users) \
         RETURNS text AS $$ \
           SELECT 'tag-' || gen_random_uuid()::text \
         $$ LANGUAGE sql VOLATILE"
    );
    db.query_raw(&create_fn)
        .await
        .expect("create function failed");

    let gql_schema = build_test_schema(&db, &schema_name).await;

    // The volatile function should not be queryable
    let query = r#"
        query {
            user(rowId: "00000000-0000-0000-0000-000000000001") {
                name
                userRandomTag
            }
        }
    "#;

    let result = gql_schema.execute(query).await;

    teardown_test_schema(&db, &schema_name).await;

    // Expect an error because the field should not exist
    assert!(
        !result.errors.is_empty(),
        "volatile function should not be exposed as a computed field"
    );
}

#[tokio::test]
async fn computed_field_with_setof_return() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    // Create a SETOF-returning stable function
    let create_fn = format!(
        "CREATE FUNCTION \"{schema_name}\".user_email_parts(u \"{schema_name}\".users) \
         RETURNS SETOF text AS $$ \
           SELECT unnest(string_to_array(u.email, '@')) \
         $$ LANGUAGE sql STABLE"
    );
    db.query_raw(&create_fn)
        .await
        .expect("create function failed");

    let gql_schema = build_test_schema(&db, &schema_name).await;

    let query = r#"
        query {
            user(rowId: "00000000-0000-0000-0000-000000000001") {
                name
                userEmailParts
            }
        }
    "#;

    let result = gql_schema.execute(query).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "setof computed field query errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    let parts = data["user"]["userEmailParts"]
        .as_array()
        .expect("should be an array");
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0], "alice");
    assert_eq!(parts[1], "example.com");
}

// ── Custom function query fields ─────────────────────────────────────────

#[tokio::test]
async fn scalar_function_as_root_query() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    // Create an IMMUTABLE scalar function (not a computed field)
    let create_fn = format!(
        "CREATE FUNCTION \"{schema_name}\".add_numbers(a integer, b integer) \
         RETURNS integer AS $$ \
           SELECT a + b \
         $$ LANGUAGE sql IMMUTABLE"
    );
    db.query_raw(&create_fn)
        .await
        .expect("create function failed");

    let gql_schema = build_test_schema(&db, &schema_name).await;

    let query = r"
        query {
            addNumbers(a: 3, b: 4)
        }
    ";

    let result = gql_schema.execute(query).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "scalar function query errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    assert_eq!(data["addNumbers"], 7);
}

#[tokio::test]
async fn setof_table_function_as_root_query() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    // Create a STABLE SETOF table function
    let create_fn = format!(
        "CREATE FUNCTION \"{schema_name}\".search_users(term text) \
         RETURNS SETOF \"{schema_name}\".users AS $$ \
           SELECT * FROM \"{schema_name}\".users WHERE name ILIKE '%' || term || '%' \
         $$ LANGUAGE sql STABLE"
    );
    db.query_raw(&create_fn)
        .await
        .expect("create function failed");

    let gql_schema = build_test_schema(&db, &schema_name).await;

    let query = r#"
        query {
            searchUsers(term: "ali", first: 10) {
                edges {
                    node {
                        name
                    }
                }
                totalCount
            }
        }
    "#;

    let result = gql_schema.execute(query).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "setof table function query errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    let edges = data["searchUsers"]["edges"]
        .as_array()
        .expect("should be an array");
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0]["node"]["name"], "Alice");
    assert_eq!(data["searchUsers"]["totalCount"], 1);
}

#[tokio::test]
async fn volatile_function_not_exposed_as_root_query() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    // Create a VOLATILE function — should NOT appear as a root query
    let create_fn = format!(
        "CREATE FUNCTION \"{schema_name}\".random_number() \
         RETURNS integer AS $$ \
           SELECT floor(random() * 100)::integer \
         $$ LANGUAGE sql VOLATILE"
    );
    db.query_raw(&create_fn)
        .await
        .expect("create function failed");

    let gql_schema = build_test_schema(&db, &schema_name).await;

    let query = r"
        query {
            randomNumber
        }
    ";

    let result = gql_schema.execute(query).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        !result.errors.is_empty(),
        "volatile function should not be exposed as a root query field"
    );
}

#[tokio::test]
async fn setof_scalar_function_as_root_query() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    // Create a STABLE SETOF scalar function
    let create_fn = format!(
        "CREATE FUNCTION \"{schema_name}\".all_user_names() \
         RETURNS SETOF text AS $$ \
           SELECT name FROM \"{schema_name}\".users ORDER BY name \
         $$ LANGUAGE sql STABLE"
    );
    db.query_raw(&create_fn)
        .await
        .expect("create function failed");

    let gql_schema = build_test_schema(&db, &schema_name).await;

    let query = r"
        query {
            allUserNames
        }
    ";

    let result = gql_schema.execute(query).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "setof scalar function query errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    let names = data["allUserNames"]
        .as_array()
        .expect("should be an array");
    assert_eq!(names.len(), 2);
    assert_eq!(names[0], "Alice");
    assert_eq!(names[1], "Bob");
}

#[tokio::test]
async fn volatile_function_as_custom_mutation() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    // Create a VOLATILE function — should appear as a mutation field
    let create_fn = format!(
        "CREATE FUNCTION \"{schema_name}\".increment_counter() \
         RETURNS integer AS $$ \
           SELECT 42 \
         $$ LANGUAGE sql VOLATILE"
    );
    db.query_raw(&create_fn)
        .await
        .expect("create function failed");

    let gql_schema = build_test_schema(&db, &schema_name).await;

    let query = r"
        mutation {
            incrementCounter
        }
    ";

    let result = gql_schema.execute(query).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "volatile function mutation errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    assert_eq!(data["incrementCounter"], 42);
}

#[tokio::test]
async fn stable_function_not_on_mutations() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    // Create a STABLE function — should NOT appear as a mutation field
    let create_fn = format!(
        "CREATE FUNCTION \"{schema_name}\".add_numbers(a integer, b integer) \
         RETURNS integer AS $$ \
           SELECT a + b \
         $$ LANGUAGE sql STABLE"
    );
    db.query_raw(&create_fn)
        .await
        .expect("create function failed");

    let gql_schema = build_test_schema(&db, &schema_name).await;

    let query = r"
        mutation {
            addNumbers(a: 1, b: 2)
        }
    ";

    let result = gql_schema.execute(query).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        !result.errors.is_empty(),
        "stable function should not be exposed as a mutation field"
    );
}

#[tokio::test]
async fn nested_create_belongs_to_input_type_exists() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    let query = r#"
        {
            __type(name: "CreatePostInput") {
                inputFields {
                    name
                    type { name kind ofType { name } }
                }
            }
        }
    "#;

    let result = gql_schema.execute(query).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "introspection errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize");
    let fields = data["__type"]["inputFields"]
        .as_array()
        .expect("inputFields should be an array");

    let field = fields
        .iter()
        .find(|f| f["name"] == "userByAuthorId")
        .expect("CreatePostInput should have a userByAuthorId field");

    assert_eq!(
        field["type"]["name"].as_str(),
        Some("PostUserByAuthorIdInput"),
        "userByAuthorId should have type PostUserByAuthorIdInput"
    );
}

#[tokio::test]
async fn nested_create_has_many_input_type_exists() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    let query = r#"
        {
            __type(name: "CreateUserInput") {
                inputFields {
                    name
                    type { name kind ofType { name } }
                }
            }
        }
    "#;

    let result = gql_schema.execute(query).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "introspection errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize");
    let fields = data["__type"]["inputFields"]
        .as_array()
        .expect("inputFields should be an array");

    let field = fields
        .iter()
        .find(|f| f["name"] == "postsByAuthorId")
        .expect("CreateUserInput should have a postsByAuthorId field");

    assert_eq!(
        field["type"]["name"].as_str(),
        Some("UserPostsByAuthorIdInput"),
        "postsByAuthorId should have type UserPostsByAuthorIdInput"
    );
}

#[tokio::test]
async fn nested_connect_input_type_has_pk_fields() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    let query = r#"
        {
            __type(name: "PostUserByAuthorIdConnectInput") {
                inputFields {
                    name
                    type { name kind ofType { name } }
                }
            }
        }
    "#;

    let result = gql_schema.execute(query).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "introspection errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize");
    let fields = data["__type"]["inputFields"]
        .as_array()
        .expect("inputFields should be an array");

    let id_field = fields
        .iter()
        .find(|f| f["name"] == "rowId")
        .expect("PostUserByAuthorIdConnectInput should have a rowId field");

    // id should be non-null (required)
    assert_eq!(
        id_field["type"]["kind"].as_str(),
        Some("NON_NULL"),
        "id field should be non-null"
    );
    assert_eq!(
        id_field["type"]["ofType"]["name"].as_str(),
        Some("UUID"),
        "id field inner type should be UUID"
    );
}

#[tokio::test]
async fn nested_create_with_belongs_to_connect() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    // Use Alice's known UUID to connect
    let mutation = r#"
        mutation {
            createPost(input: {
                title: "Nested Connect Test"
                content: "Testing"
                userByAuthorId: { connect: { rowId: "00000000-0000-0000-0000-000000000001" } }
            }) {
                post {
                    title
                    userByAuthorId { name }
                }
            }
        }
    "#;

    let result = gql_schema.execute(mutation).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "nested connect errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize");
    let post = &data["createPost"]["post"];
    assert_eq!(post["title"], "Nested Connect Test");
    assert_eq!(post["userByAuthorId"]["name"], "Alice");
}

#[tokio::test]
async fn nested_create_with_belongs_to_create() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    let mutation = r#"
        mutation {
            createPost(input: {
                title: "Nested Create Test"
                content: "Testing"
                userByAuthorId: {
                    create: {
                        name: "InlineUser"
                        email: "inline@test.com"
                    }
                }
            }) {
                post {
                    title
                    userByAuthorId { name email }
                }
            }
        }
    "#;

    let result = gql_schema.execute(mutation).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "nested create errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize");
    let post = &data["createPost"]["post"];
    assert_eq!(post["title"], "Nested Create Test");
    assert_eq!(post["userByAuthorId"]["name"], "InlineUser");
    assert_eq!(post["userByAuthorId"]["email"], "inline@test.com");
}

#[tokio::test]
async fn nested_create_with_has_many_create() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    let mutation = r#"
        mutation {
            createPost(input: {
                title: "Post With Comments"
                content: "Testing"
                authorId: "00000000-0000-0000-0000-000000000001"
                commentsByPostId: {
                    create: [
                        { body: "Comment 1", authorId: "00000000-0000-0000-0000-000000000001" }
                        { body: "Comment 2", authorId: "00000000-0000-0000-0000-000000000001" }
                    ]
                }
            }) {
                post {
                    title
                    commentsByPostId { edges { node { body } } }
                }
            }
        }
    "#;

    let result = gql_schema.execute(mutation).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "nested has-many create errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize");
    let post = &data["createPost"]["post"];
    assert_eq!(post["title"], "Post With Comments");

    let edges = post["commentsByPostId"]["edges"]
        .as_array()
        .expect("edges should be an array");
    assert_eq!(edges.len(), 2, "should have 2 comments");

    let bodies: Vec<&str> = edges
        .iter()
        .map(|e| e["node"]["body"].as_str().unwrap())
        .collect();
    assert!(bodies.contains(&"Comment 1"));
    assert!(bodies.contains(&"Comment 2"));
}

#[tokio::test]
async fn nested_create_combined_belongs_to_and_has_many() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    let mutation = r#"
        mutation {
            createPost(input: {
                title: "Full Nested Test"
                content: "Both directions"
                userByAuthorId: {
                    create: { name: "NestedAuthor", email: "nested@test.com" }
                }
                commentsByPostId: {
                    create: [
                        { body: "Nested comment", authorId: "00000000-0000-0000-0000-000000000001" }
                    ]
                }
            }) {
                post {
                    title
                    userByAuthorId { name }
                    commentsByPostId { edges { node { body } } }
                }
            }
        }
    "#;

    let result = gql_schema.execute(mutation).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "combined nested create errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize");
    let post = &data["createPost"]["post"];
    assert_eq!(post["title"], "Full Nested Test");
    assert_eq!(post["userByAuthorId"]["name"], "NestedAuthor");

    let edges = post["commentsByPostId"]["edges"]
        .as_array()
        .expect("edges should be an array");
    assert_eq!(edges.len(), 1, "should have 1 comment");
    assert_eq!(edges[0]["node"]["body"], "Nested comment");
}

#[tokio::test]
async fn nested_create_rolls_back_on_child_failure() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    // Count users before
    let count_before = db
        .query_raw(&format!(
            "SELECT COUNT(*)::int AS cnt FROM \"{schema_name}\".users"
        ))
        .await
        .expect("count query failed");
    let user_count_before = count_before[0]["cnt"].as_i64().unwrap();

    // Attempt to create a post with an inline author AND a comment with null
    // body (body is NOT NULL), which should fail and roll back everything
    let mutation = r#"
        mutation {
            createPost(input: {
                title: "Should Rollback"
                content: "Testing rollback"
                userByAuthorId: {
                    create: { name: "RollbackUser", email: "rollback@test.com" }
                }
                commentsByPostId: {
                    create: [
                        { authorId: "00000000-0000-0000-0000-000000000001" }
                    ]
                }
            }) {
                post { title }
            }
        }
    "#;

    let result = gql_schema.execute(mutation).await;

    assert!(
        !result.errors.is_empty(),
        "mutation should fail due to missing required body"
    );

    // Verify rollback: user count should be unchanged
    let count_after = db
        .query_raw(&format!(
            "SELECT COUNT(*)::int AS cnt FROM \"{schema_name}\".users"
        ))
        .await
        .expect("count query failed");
    let user_count_after = count_after[0]["cnt"].as_i64().unwrap();

    teardown_test_schema(&db, &schema_name).await;

    assert_eq!(
        user_count_before, user_count_after,
        "user count should be unchanged after rollback (before={user_count_before}, after={user_count_after})"
    );
}

#[tokio::test]
async fn nested_create_connect_nonexistent_returns_error() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    let mutation = r#"
        mutation {
            createPost(input: {
                title: "Bad Connect"
                content: "Testing"
                userByAuthorId: {
                    connect: { rowId: "ffffffff-ffff-ffff-ffff-ffffffffffff" }
                }
            }) {
                post { title }
            }
        }
    "#;

    let result = gql_schema.execute(mutation).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        !result.errors.is_empty(),
        "connecting to nonexistent record should fail"
    );

    let error_msg = result.errors[0].message.to_lowercase();
    assert!(
        error_msg.contains("record not found"),
        "error should mention 'record not found', got: {error_msg}"
    );
}

#[tokio::test]
async fn nested_mutation_respects_omit_create_on_related_table() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    // Add @omit(create) to the users table
    db.query_raw(&format!(
        "COMMENT ON TABLE \"{schema_name}\".users IS E'@omit(create)'"
    ))
    .await
    .unwrap();

    // Re-introspect and rebuild the schema with the new comment
    let gql_schema = build_test_schema(&db, &schema_name).await;

    let sdl = gql_schema.sdl();

    // PostUserByAuthorIdInput should still exist (for connect)
    assert!(
        sdl.contains("PostUserByAuthorIdInput"),
        "PostUserByAuthorIdInput should exist for connect-only"
    );

    // Introspect PostUserByAuthorIdInput to verify it has connect but NOT create
    let query = r#"
        {
            __type(name: "PostUserByAuthorIdInput") {
                inputFields { name }
            }
        }
    "#;

    let result = gql_schema.execute(query).await;
    assert!(result.errors.is_empty(), "introspection errors: {:?}", result.errors);

    let data = result.data.into_json().expect("failed to serialize");
    let fields = data["__type"]["inputFields"]
        .as_array()
        .expect("inputFields should be an array");

    let field_names: Vec<&str> = fields
        .iter()
        .filter_map(|f| f["name"].as_str())
        .collect();

    assert!(
        field_names.contains(&"connect"),
        "PostUserByAuthorIdInput should have connect field"
    );
    assert!(
        !field_names.contains(&"create"),
        "PostUserByAuthorIdInput should NOT have create field when related table has @omit(create)"
    );

    // CreateUserInput should not exist since users has @omit(create)
    assert!(
        !sdl.contains("CreateUserInput"),
        "CreateUserInput should not exist when users has @omit(create)"
    );

    // Has-many nested inputs from users should also be suppressed:
    // CreatePostInput should NOT have a has-many nested field for users
    // (users table has @omit(create), so it won't have CreateUserInput
    // to nest into anyway, but the PostCommentsByPostIdInput should still exist)
    assert!(
        sdl.contains("PostCommentsByPostIdInput"),
        "PostCommentsByPostIdInput should still exist (comments not omitted)"
    );

    teardown_test_schema(&db, &schema_name).await;
}

/// Plugin that adds a custom `userCount` root query backed by SQL, exercising
/// the schema-extension hook.
struct UserCountPlugin {
    schema: String,
}

#[allow(clippy::unnecessary_literal_bound)]
impl Plugin for UserCountPlugin {
    fn name(&self) -> &str {
        "user-count"
    }

    fn query_fields(
        &self,
        _schemas: &[taproot_core::introspect::IntrospectedSchema],
    ) -> Vec<async_graphql::dynamic::Field> {
        let schema = self.schema.clone();
        vec![async_graphql::dynamic::Field::new(
            "userCount",
            async_graphql::dynamic::TypeRef::named(async_graphql::dynamic::TypeRef::INT),
            move |ctx| {
                let schema = schema.clone();
                async_graphql::dynamic::FieldFuture::new(async move {
                    let db = ctx.data::<Arc<dyn Database>>()?;
                    let rls = ctx.data::<RlsContext>().ok();
                    let sql = format!("SELECT COUNT(*) AS count FROM \"{schema}\".users");
                    let rows = db
                        .query_with_rls(&sql, &[], rls)
                        .await
                        .map_err(|e| async_graphql::Error::new(e.to_string()))?;
                    let count = rows
                        .first()
                        .and_then(|r| r.get("count"))
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0);
                    Ok(Some(async_graphql::dynamic::FieldValue::value(
                        async_graphql::Value::from(count),
                    )))
                })
            },
        )]
    }
}

#[tokio::test]
async fn plugin_adds_custom_sql_backed_query_field() {
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
    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(UserCountPlugin {
        schema: schema_name.clone(),
    })];

    let gql_schema =
        build_schema(&introspected, &db_arc, &SchemaOptions::default(), &plugins, None)
            .expect("schema build");

    // The custom field shows up in the schema and resolves via SQL
    assert!(gql_schema.sdl().contains("userCount"), "userCount missing from SDL");

    let result = gql_schema.execute("{ userCount }").await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(result.errors.is_empty(), "userCount errors: {:?}", result.errors);
    let data = result.data.into_json().expect("serialize");
    assert_eq!(data["userCount"], 2);
}

#[tokio::test]
async fn relay_query_nested_root() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    // The nested `query` root returns the Query type, so fields re-resolve
    let result = gql_schema
        .execute("{ query { users { totalCount } } }")
        .await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(result.errors.is_empty(), "nested query errors: {:?}", result.errors);
    let data = result.data.into_json().expect("serialize");
    assert_eq!(data["query"]["users"]["totalCount"], 2);
}

#[tokio::test]
async fn condition_flat_equality() {
    let Some(db) = connect().await else {
        return;
    };
    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;
    let gql_schema = build_test_schema(&db, &schema_name).await;

    // `condition` is flat equality: { field: value }
    let query = r#"{ users(condition: { name: "Alice" }) { nodes { name } totalCount } }"#;
    let result = gql_schema.execute(query).await;
    teardown_test_schema(&db, &schema_name).await;

    assert!(result.errors.is_empty(), "condition errors: {:?}", result.errors);
    let data = result.data.into_json().expect("serialize");
    assert_eq!(data["users"]["totalCount"], 1);
    assert_eq!(data["users"]["nodes"][0]["name"], "Alice");
}

#[tokio::test]
async fn filter_rich_operators_and_combinators() {
    let Some(db) = connect().await else {
        return;
    };
    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;
    let gql_schema = build_test_schema(&db, &schema_name).await;

    // `or` combinator over two equality filters
    let or_query = r#"{ users(filter: { or: [{ name: { equalTo: "Alice" } }, { name: { equalTo: "Bob" } }] }) { totalCount } }"#;
    let or_result = gql_schema.execute(or_query).await;
    assert!(or_result.errors.is_empty(), "or errors: {:?}", or_result.errors);
    assert_eq!(or_result.data.into_json().unwrap()["users"]["totalCount"], 2);

    // case-insensitive substring match
    let ci_query = r#"{ users(filter: { email: { includesInsensitive: "EXAMPLE" } }) { totalCount } }"#;
    let ci_result = gql_schema.execute(ci_query).await;
    assert!(ci_result.errors.is_empty(), "ci errors: {:?}", ci_result.errors);
    assert_eq!(ci_result.data.into_json().unwrap()["users"]["totalCount"], 2);

    // not combinator excludes Alice
    let not_query = r#"{ users(filter: { not: { name: { equalTo: "Alice" } } }) { nodes { name } totalCount } }"#;
    let not_result = gql_schema.execute(not_query).await;
    teardown_test_schema(&db, &schema_name).await;
    assert!(not_result.errors.is_empty(), "not errors: {:?}", not_result.errors);
    let not_data = not_result.data.into_json().unwrap();
    assert_eq!(not_data["users"]["totalCount"], 1);
    assert_eq!(not_data["users"]["nodes"][0]["name"], "Bob");
}

#[tokio::test]
async fn array_column_filter() {
    let Some(db) = connect().await else {
        return;
    };
    let schema_name = test_schema();
    db.query_raw(&format!("CREATE SCHEMA IF NOT EXISTS \"{schema_name}\""))
        .await
        .expect("create schema");
    db.query_raw(&format!(
        "CREATE TABLE \"{schema_name}\".items (id uuid PRIMARY KEY DEFAULT gen_random_uuid(), name text NOT NULL, tags text[] NOT NULL DEFAULT '{{}}')"
    ))
    .await
    .expect("create table");
    db.query_raw(&format!(
        "INSERT INTO \"{schema_name}\".items (name, tags) VALUES ('a', ARRAY['red','blue']), ('b', ARRAY['green']), ('c', ARRAY['blue','green'])"
    ))
    .await
    .expect("seed");

    let gql_schema = build_test_schema(&db, &schema_name).await;

    // ListFilter type exists for the array column's element scalar
    assert!(gql_schema.sdl().contains("StringListFilter"), "StringListFilter missing from SDL");

    // anyEqualTo: items whose tags contain 'blue' -> a and c
    let q = r#"{ items(filter: { tags: { anyEqualTo: "blue" } }) { totalCount nodes { name } } }"#;
    let result = gql_schema.execute(q).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(result.errors.is_empty(), "array filter errors: {:?}", result.errors);
    let data = result.data.into_json().expect("serialize");
    assert_eq!(data["items"]["totalCount"], 2, "expected 2 items tagged blue");
    let names: Vec<&str> = data["items"]["nodes"].as_array().unwrap().iter().map(|n| n["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"a") && names.contains(&"c"), "got {names:?}");
}
