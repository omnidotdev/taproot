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
    /// Whether the `GraphiQL` playground is enabled.
    pub playground: bool,
}

/// Build the axum router with `GraphQL` and health check routes.
pub fn build_router(schema: Schema, config: &ServerConfig, auth: &AuthConfig) -> Router {
    let gql_path = config.graphql_path.clone();
    let ws_path = format!("{gql_path}/ws");

    let state = AppState {
        schema: Arc::new(schema.clone()),
        auth: auth.clone(),
        graphql_path: gql_path.clone(),
        playground: config.playground,
    };

    Router::new()
        .route(&gql_path, get(graphql_get_or_playground).post(graphql_post))
        .route_service(&ws_path, GraphQLSubscription::new(schema))
        .route("/health", get(health))
        .layer(CorsLayer::permissive())
        .with_state(state)
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

/// Handle `GET /graphql` — serve `GraphiQL` for browsers, execute query for API clients.
async fn graphql_get_or_playground(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    params: Result<axum::extract::Query<GraphQlGetParams>, axum::extract::rejection::QueryRejection>,
) -> axum::response::Response {
    let query = params.map(|q| q.0.query).unwrap_or_default();

    // Serve GraphiQL if the request accepts HTML and has no query
    let accepts_html = headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/html"));

    if state.playground && accepts_html && query.is_empty() {
        return Html(
            GraphiQLSource::build()
                .endpoint(&state.graphql_path)
                .finish(),
        )
        .into_response();
    }

    if query.is_empty() {
        return Json(serde_json::json!({"errors": [{"message": "Missing query parameter"}]}))
            .into_response();
    }

    let rls = build_rls_context(&state.auth, &headers);
    let request = async_graphql::Request::new(&query).data(rls);
    let response = state.schema.execute(request).await;
    Json(response).into_response()
}

/// Query parameters for `GET /graphql`.
#[derive(serde::Deserialize)]
struct GraphQlGetParams {
    #[serde(default)]
    query: String,
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
    // Enforce expiry when present, but do not require it: a token carrying an
    // expired exp is rejected, while a token with no exp is still accepted
    validation.validate_exp = true;
    validation.required_spec_claims.clear();

    let data = decode::<serde_json::Value>(token, &key, &validation).ok()?;
    serde_json::to_string(&data.claims).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use jsonwebtoken::{EncodingKey, Header, encode};

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn sign(claims: &serde_json::Value, secret: &str) -> String {
        encode(
            &Header::default(),
            claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap()
    }

    fn bearer(headers: &mut HeaderMap, token: &str) {
        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
    }

    #[test]
    fn unverified_decode_extracts_payload() {
        let token = sign(&serde_json::json!({ "pg_role": "app_user" }), "s");
        let json = decode_jwt_unverified(&token).expect("payload");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["pg_role"], "app_user");
    }

    #[test]
    fn unverified_decode_rejects_malformed() {
        assert!(decode_jwt_unverified("not-a-jwt").is_none());
        assert!(decode_jwt_unverified("only.two").is_none());
    }

    #[test]
    fn verified_decode_accepts_valid_without_exp() {
        let token = sign(&serde_json::json!({ "pg_role": "app_user" }), "secret");
        assert!(decode_jwt_verified(&token, "secret").is_some());
    }

    #[test]
    fn verified_decode_accepts_unexpired() {
        let token = sign(
            &serde_json::json!({ "pg_role": "app_user", "exp": now_secs() + 3600 }),
            "secret",
        );
        assert!(decode_jwt_verified(&token, "secret").is_some());
    }

    #[test]
    fn verified_decode_rejects_expired() {
        let token = sign(
            &serde_json::json!({ "pg_role": "app_user", "exp": now_secs() - 3600 }),
            "secret",
        );
        assert!(
            decode_jwt_verified(&token, "secret").is_none(),
            "an expired token must be rejected"
        );
    }

    #[test]
    fn verified_decode_rejects_wrong_secret() {
        let token = sign(&serde_json::json!({ "pg_role": "app_user" }), "right");
        assert!(decode_jwt_verified(&token, "wrong").is_none());
    }

    #[test]
    fn rls_context_empty_when_disabled() {
        let auth = AuthConfig::default();
        let ctx = build_rls_context(&auth, &HeaderMap::new());
        assert!(ctx.role.is_none());
        assert!(ctx.claims_json.is_none());
    }

    #[test]
    fn rls_context_default_role_without_token() {
        let auth = AuthConfig { rls: true, ..Default::default() };
        let ctx = build_rls_context(&auth, &HeaderMap::new());
        assert_eq!(ctx.role.as_deref(), Some("anonymous"));
        assert!(ctx.claims_json.is_none());
    }

    #[test]
    fn rls_context_role_from_claim() {
        let auth = AuthConfig {
            rls: true,
            jwt_secret: Some("secret".into()),
            ..Default::default()
        };
        let token = sign(&serde_json::json!({ "pg_role": "app_user" }), "secret");
        let mut headers = HeaderMap::new();
        bearer(&mut headers, &token);
        let ctx = build_rls_context(&auth, &headers);
        assert_eq!(ctx.role.as_deref(), Some("app_user"));
        assert!(ctx.claims_json.is_some());
    }

    #[test]
    fn rls_context_expired_token_falls_back_to_default_role() {
        let auth = AuthConfig {
            rls: true,
            jwt_secret: Some("secret".into()),
            ..Default::default()
        };
        let token = sign(
            &serde_json::json!({ "pg_role": "app_user", "exp": now_secs() - 3600 }),
            "secret",
        );
        let mut headers = HeaderMap::new();
        bearer(&mut headers, &token);
        let ctx = build_rls_context(&auth, &headers);
        assert_eq!(ctx.role.as_deref(), Some("anonymous"));
    }

    #[test]
    fn rls_context_non_bearer_header_uses_default_role() {
        let auth = AuthConfig { rls: true, ..Default::default() };
        let mut headers = HeaderMap::new();
        // Missing the "Bearer " prefix: treated as no token
        headers.insert("authorization", "Basic abc123".parse().unwrap());
        let ctx = build_rls_context(&auth, &headers);
        assert_eq!(ctx.role.as_deref(), Some("anonymous"));
        assert!(ctx.claims_json.is_none());
    }

    #[test]
    fn rls_context_unverified_mode_extracts_role() {
        // No jwt_secret => trust an upstream-verified token, decode without
        // checking the signature, but still extract the role claim
        let auth = AuthConfig { rls: true, jwt_secret: None, ..Default::default() };
        let token = sign(&serde_json::json!({ "pg_role": "app_user" }), "whatever");
        let mut headers = HeaderMap::new();
        bearer(&mut headers, &token);
        let ctx = build_rls_context(&auth, &headers);
        assert_eq!(ctx.role.as_deref(), Some("app_user"));
        assert!(ctx.claims_json.is_some());
    }

    #[test]
    fn rls_context_missing_role_claim_uses_default() {
        let auth = AuthConfig {
            rls: true,
            jwt_secret: Some("secret".into()),
            ..Default::default()
        };
        let token = sign(&serde_json::json!({ "sub": "u1" }), "secret");
        let mut headers = HeaderMap::new();
        bearer(&mut headers, &token);
        let ctx = build_rls_context(&auth, &headers);
        assert_eq!(ctx.role.as_deref(), Some("anonymous"));
        // Claims are still passed through to Postgres even without a role
        assert!(ctx.claims_json.is_some());
    }

    #[test]
    fn rls_context_non_string_role_claim_uses_default() {
        let auth = AuthConfig {
            rls: true,
            jwt_secret: Some("secret".into()),
            ..Default::default()
        };
        let token = sign(&serde_json::json!({ "pg_role": 42 }), "secret");
        let mut headers = HeaderMap::new();
        bearer(&mut headers, &token);
        let ctx = build_rls_context(&auth, &headers);
        assert_eq!(ctx.role.as_deref(), Some("anonymous"));
    }
}
