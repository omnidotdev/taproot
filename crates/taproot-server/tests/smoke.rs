use std::sync::Arc;

use taproot_core::db::{Database, SqlxBackend};
use taproot_core::introspect::types::IntrospectOptions;
use taproot_core::registry::types::SchemaOptions;
use taproot_core::{build_schema, introspect};

/// Connect to test database, returning None if `DATABASE_URL` is unset.
async fn connect() -> Option<SqlxBackend> {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL not set -- skipping smoke tests");
        return None;
    };
    Some(SqlxBackend::connect(&url).await.expect("failed to connect"))
}

#[tokio::test]
async fn server_handles_graphql_post() {
    let Some(db) = connect().await else { return };

    // Create test schema
    let schema_name = format!("smoke_test_{}", std::process::id());
    let ddl = [
        format!(r#"CREATE SCHEMA IF NOT EXISTS "{schema_name}""#),
        format!(
            r#"CREATE TABLE "{schema_name}".items (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                name TEXT NOT NULL
            )"#
        ),
        format!(
            r#"INSERT INTO "{schema_name}".items (id, name) VALUES
                ('00000000-0000-0000-0000-000000000001', 'Widget')"#
        ),
    ];

    for stmt in &ddl {
        db.query_raw(stmt).await.expect("DDL failed");
    }

    // Build schema
    let db_arc: Arc<dyn Database> = Arc::new(db.clone());
    let introspected = introspect(
        db_arc.as_ref(),
        &IntrospectOptions {
            schemas: vec![schema_name.clone()],
            ..Default::default()
        },
    )
    .await
    .expect("introspection");

    let gql_schema = build_schema(
        &introspected,
        &db_arc,
        &SchemaOptions::default(),
        &[],
        None,
    )
    .expect("schema build");

    // Start server on random port
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let config = taproot_server::config::ServerConfig::default();
    let auth = taproot_server::config::AuthConfig::default();
    let router = taproot_server::server::build_router(gql_schema, &config, &auth);

    tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });

    // Send a real HTTP request
    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{addr}/graphql"))
        .json(&serde_json::json!({
            "query": "{ items(first: 10) { edges { node { name } } } }"
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(response.status(), 200);

    let body: serde_json::Value = response.json().await.expect("parse json");
    let edges = body["data"]["items"]["edges"]
        .as_array()
        .expect("edges array");
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0]["node"]["name"], "Widget");

    // Also test health endpoint
    let health = client
        .get(format!("http://{addr}/health"))
        .send()
        .await
        .expect("health request");
    assert_eq!(health.status(), 200);
    assert_eq!(health.text().await.unwrap(), "ok");

    // Cleanup
    let _ = db
        .query_raw(&format!(r#"DROP SCHEMA "{schema_name}" CASCADE"#))
        .await;
}
