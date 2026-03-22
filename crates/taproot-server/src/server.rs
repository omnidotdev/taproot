use std::sync::Arc;

use async_graphql::dynamic::Schema;
use async_graphql::http::GraphiQLSource;
use async_graphql_axum::GraphQLSubscription;
use axum::extract::State;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};
use tower_http::cors::CorsLayer;

use crate::config::{AuthConfig, ServerConfig};
use taproot_core::db::RlsContext;

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    /// The dynamic `GraphQL` schema.
    pub schema: Arc<Schema>,
    /// Authentication / RLS configuration.
    pub auth: AuthConfig,
    /// GraphQL endpoint path.
    pub graphql_path: String,
}

/// Build the axum router with `GraphQL` and health check routes.
pub fn build_router(schema: Schema, config: &ServerConfig, auth: &AuthConfig) -> Router {
    let gql_path = config.graphql_path.clone();
    let ws_path = format!("{gql_path}/ws");

    let state = AppState {
        schema: Arc::new(schema.clone()),
        auth: auth.clone(),
        graphql_path: gql_path.clone(),
    };

    let mut router = Router::new()
        .route(&gql_path, get(graphql_get).post(graphql_post))
        .route_service(&ws_path, GraphQLSubscription::new(schema))
        .route("/health", get(health));

    if config.playground {
        router = router.route("/", get(graphiql));
    }

    router.layer(CorsLayer::permissive()).with_state(state)
}

/// Handle `POST /graphql` requests.
async fn graphql_post(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(request): Json<async_graphql::Request>,
) -> impl IntoResponse {
    let rls = build_rls_context(&state.auth, &headers);
    let request = request.data(rls);
    let response = state.schema.execute(request).await;
    Json(response)
}

/// Handle `GET /graphql` requests (query via query string).
async fn graphql_get(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    req: axum::extract::Query<GraphQlGetParams>,
) -> impl IntoResponse {
    let rls = build_rls_context(&state.auth, &headers);
    let request = async_graphql::Request::new(&req.query).data(rls);
    let response = state.schema.execute(request).await;
    Json(response)
}

/// Query parameters for `GET /graphql`.
#[derive(serde::Deserialize)]
struct GraphQlGetParams {
    query: String,
}

/// Serve the `GraphiQL` playground.
async fn graphiql(State(state): State<AppState>) -> impl IntoResponse {
    Html(
        GraphiQLSource::build()
            .endpoint(&state.graphql_path)
            .finish(),
    )
}

/// Health check endpoint.
async fn health() -> &'static str {
    "ok"
}

/// Build an `RlsContext` from the auth config and request headers.
fn build_rls_context(auth: &AuthConfig, headers: &axum::http::HeaderMap) -> RlsContext {
    if !auth.rls {
        return RlsContext::default();
    }

    // Extract Bearer token
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");

    if token.is_empty() {
        return RlsContext {
            role: Some(auth.default_role.clone()),
            claims_json: None,
            claims_setting: auth.claims_setting.clone(),
        };
    }

    // Decode JWT
    let claims_json = auth.jwt_secret.as_ref().map_or_else(
        || decode_jwt_unverified(token),
        |secret| decode_jwt_verified(token, secret),
    );

    let role = claims_json
        .as_ref()
        .and_then(|json| {
            serde_json::from_str::<serde_json::Value>(json)
                .ok()
                .and_then(|v| {
                    v.get(&auth.role_claim)
                        .and_then(|r| r.as_str().map(String::from))
                })
        })
        .unwrap_or_else(|| auth.default_role.clone());

    RlsContext {
        role: Some(role),
        claims_json,
        claims_setting: auth.claims_setting.clone(),
    }
}

/// Decode JWT payload without signature verification.
fn decode_jwt_unverified(token: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .ok()?;
    String::from_utf8(payload).ok()
}

/// Decode and verify JWT with HS256 secret.
fn decode_jwt_verified(token: &str, secret: &str) -> Option<String> {
    let key = DecodingKey::from_secret(secret.as_bytes());
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = false;
    validation.required_spec_claims.clear();

    let data = decode::<serde_json::Value>(token, &key, &validation).ok()?;
    serde_json::to_string(&data.claims).ok()
}
