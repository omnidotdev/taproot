use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use taproot_core::db::{Database, SqlxBackend};
use taproot_core::registry::types::SchemaOptions;
use taproot_core::{IntrospectOptions, build_schema, introspect};

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

/// Connect to the test database, returning None if DATABASE_URL is unset.
async fn connect() -> Option<SqlxBackend> {
    let url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            eprintln!("DATABASE_URL not set -- skipping integration tests");
            return None;
        }
    };

    Some(
        SqlxBackend::connect(&url)
            .await
            .expect("failed to connect to test database"),
    )
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

    let options = IntrospectOptions {
        schemas: vec![schema_name.clone()],
        exclude_tables: Vec::new(),
        smart_comments: true,
    };

    let introspected = introspect(&db, &options)
        .await
        .expect("introspection failed");

    let db_arc: Arc<dyn Database> = Arc::new(db.clone());
    let schema_opts = SchemaOptions::default();
    let result = build_schema(&introspected, &db_arc, &schema_opts);

    teardown_test_schema(&db, &schema_name).await;

    let gql_schema = result.expect("schema build failed");
    let sdl = gql_schema.sdl();

    assert!(sdl.contains("User"), "User type missing from SDL");
    assert!(sdl.contains("Post"), "Post type missing from SDL");
    assert!(sdl.contains("UserRole"), "UserRole enum missing from SDL");
    assert!(sdl.contains("users"), "users query field missing from SDL");
    assert!(sdl.contains("posts"), "posts query field missing from SDL");
}

#[tokio::test]
async fn execute_list_query() {
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

    let introspected = introspect(&db, &options)
        .await
        .expect("introspection failed");

    let db_arc: Arc<dyn Database> = Arc::new(db.clone());
    let schema_opts = SchemaOptions::default();
    let gql_schema =
        build_schema(&introspected, &db_arc, &schema_opts).expect("schema build failed");

    let result = gql_schema.execute("{ users { name email } }").await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(result.errors.is_empty(), "query errors: {:?}", result.errors);

    let users = result.data.into_json().expect("failed to serialize result");
    let list = users
        .get("users")
        .and_then(|v| v.as_array())
        .expect("users field missing or not an array");
    assert_eq!(list.len(), 2);

    let names: Vec<&str> = list
        .iter()
        .filter_map(|u| u.get("name").and_then(|v| v.as_str()))
        .collect();
    assert!(names.contains(&"Alice"));
    assert!(names.contains(&"Bob"));
}

#[tokio::test]
async fn execute_singular_query_by_id() {
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

    let introspected = introspect(&db, &options)
        .await
        .expect("introspection failed");

    let db_arc: Arc<dyn Database> = Arc::new(db.clone());
    let schema_opts = SchemaOptions::default();
    let gql_schema =
        build_schema(&introspected, &db_arc, &schema_opts).expect("schema build failed");

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
