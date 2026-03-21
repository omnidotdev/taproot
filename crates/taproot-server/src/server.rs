use std::sync::Arc;

use async_graphql::dynamic::Schema;
use async_graphql::http::GraphiQLSource;
use axum::extract::State;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Json, Router};
use tower_http::cors::CorsLayer;

use crate::config::ServerConfig;

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    /// The dynamic `GraphQL` schema.
    pub schema: Arc<Schema>,
}

/// Build the axum router with `GraphQL` and health check routes.
pub fn build_router(schema: Schema, config: &ServerConfig) -> Router {
    let state = AppState {
        schema: Arc::new(schema),
    };

    let mut router = Router::new()
        .route("/graphql", get(graphql_get).post(graphql_post))
        .route("/health", get(health));

    if config.playground {
        router = router.route("/", get(graphiql));
    }

    router.layer(CorsLayer::permissive()).with_state(state)
}

/// Handle `POST /graphql` requests.
async fn graphql_post(
    State(state): State<AppState>,
    Json(request): Json<async_graphql::Request>,
) -> impl IntoResponse {
    let response = state.schema.execute(request).await;
    Json(response)
}

/// Handle `GET /graphql` requests (query via query string).
async fn graphql_get(
    State(state): State<AppState>,
    req: axum::extract::Query<GraphQlGetParams>,
) -> impl IntoResponse {
    let request = async_graphql::Request::new(&req.query);
    let response = state.schema.execute(request).await;
    Json(response)
}

/// Query parameters for `GET /graphql`.
#[derive(serde::Deserialize)]
struct GraphQlGetParams {
    query: String,
}

/// Serve the `GraphiQL` playground.
async fn graphiql() -> impl IntoResponse {
    Html(
        GraphiQLSource::build()
            .endpoint("/graphql")
            .finish(),
    )
}

/// Health check endpoint.
async fn health() -> &'static str {
    "ok"
}
