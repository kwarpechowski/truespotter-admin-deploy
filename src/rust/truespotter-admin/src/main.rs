//! truespotter-admin HTTP service (bin) — axum 0.7, PORT 8089.
//!
//! Read-only admin surface for the RN app (AdminService.ts), ported from
//! the .NET API: POST /api/admin/login (x-admin-secret → 1h JWT),
//! GET /api/admin/chat/sessions, GET /api/products (OpenSearch catalog),
//! GET /api/web-reviews, GET /api/admin/stats, GET /api/workflows{,/{id},
//! /{id}/history} (Temporal SQL visibility). Write endpoints stay in .NET.
//! Same runtime pattern as truespotter-core/-edge: `--healthcheck`
//! self-probe, tracing_subscriber init.

mod auth;
pub use auth::admin_guard;
mod config;
mod products;
mod sessions;
mod stats;
mod web_reviews;
mod workflows;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::sync::Arc;

pub struct AppState {
    pub db: PgPool,
    pub metrics_secret: Option<String>,
    pub jwt_secret: Option<String>,
    pub opensearch_url: String,
    pub opensearch_index: String,
    pub opensearch_user: Option<String>,
    pub opensearch_password: Option<String>,
    pub http_client: reqwest::Client,
}

/// Results.BadRequest("msg"): plain text body (unused so far — kept for
/// parity helpers; the guard writes bare 401 "Unauthorized" text).
#[allow(dead_code)]
pub fn text(status: StatusCode, body: &str) -> Response {
    (
        status,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        body.to_string(),
    )
        .into_response()
}

/// Results.Problem(detail, statusCode): RFC 7807 ProblemDetails JSON —
/// the shape ASP.NET emits for `Results.Problem`.
pub fn problem(status: StatusCode, detail: &str) -> Response {
    let section = match status.as_u16() {
        400 => "https://tools.ietf.org/html/rfc7231#section-6.5.1",
        404 => "https://tools.ietf.org/html/rfc7231#section-6.5.4",
        500 => "https://tools.ietf.org/html/rfc7231#section-6.6.1",
        503 => "https://tools.ietf.org/html/rfc7231#section-6.6.4",
        _ => "https://tools.ietf.org/html/rfc7231#section-6.6.1",
    };
    let body = serde_json::json!({
        "type": section,
        "title": "An error occurred while processing your request.",
        "status": status.as_u16(),
        "detail": detail,
    });
    (status, Json(body)).into_response()
}

/// The .NET `DateTime` (Kind=Utc) wire format used by System.Text.Json:
/// ISO 8601 with up to 7 fractional digits (100ns ticks) and trailing
/// zeros trimmed (same helper as truespotter-core).
pub fn format_dotnet_utc(dt: &DateTime<Utc>) -> String {
    let ticks = (dt.timestamp_subsec_nanos() / 100) as i64;
    let mut frac = format!("{:07}", ticks);
    while frac.ends_with('0') {
        frac.pop();
    }
    let base = dt.format("%Y-%m-%dT%H:%M:%S");
    if frac.is_empty() {
        format!("{}Z", base)
    } else {
        format!("{}.{}Z", base, frac)
    }
}

/// A `DateTime<Utc>` that serializes exactly like .NET System.Text.Json.
#[derive(Debug, Clone, PartialEq)]
pub struct DotNetTime(pub DateTime<Utc>);

impl serde::Serialize for DotNetTime {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format_dotnet_utc(&self.0))
    }
}

#[cfg(test)]
impl Default for DotNetTime {
    fn default() -> Self {
        DotNetTime(chrono::TimeZone::timestamp_opt(&Utc, 0, 0).unwrap())
    }
}

async fn health() -> Response {
    Json(serde_json::json!({ "status": "ok" })).into_response()
}

fn build_router(state: Arc<AppState>) -> Router {
    // Protected read surface — the UseAdminAuth guard runs as middleware
    // on every route below (Bearer JWT with admin role OR x-admin-secret
    // OR ?key=). Login and health sit OUTSIDE the guard layer.
    let protected = Router::new()
        .route(
            "/api/admin/chat/sessions",
            get(sessions::list_sessions_handler),
        )
        .route("/api/products", get(products::list_products_handler))
        .route(
            "/api/web-reviews",
            get(web_reviews::list_web_reviews_handler),
        )
        .route("/api/admin/stats", get(stats::stats_handler))
        .route("/api/workflows", get(workflows::list_workflows_handler))
        .route("/api/workflows/:id", get(workflows::get_workflow_handler))
        .route(
            "/api/workflows/:id/history",
            get(workflows::workflow_history_handler),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::admin_middleware,
        ));

    Router::new()
        .merge(protected)
        .route("/api/admin/login", post(auth::login_handler))
        .route("/health", get(health))
        .with_state(state)
}

/// GET /health over a raw TCP socket (scratch image has no curl/shell).
async fn healthcheck_probe(port: u16) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
        Ok(s) => s,
        Err(_) => return false,
    };
    if stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .await
        .is_err()
    {
        return false;
    }
    let mut buf = [0u8; 32];
    let n = stream.read(&mut buf).await.unwrap_or(0);
    buf[..n].starts_with(b"HTTP/1.1 200")
}

#[tokio::main]
async fn main() {
    if std::env::args().any(|a| a == "--healthcheck") {
        let port: u16 = std::env::var("PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(8089);
        let ok = healthcheck_probe(port).await;
        std::process::exit(if ok { 0 } else { 1 });
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "truespotter_admin=info".into()),
        )
        .init();

    let config = config::Config::from_env();
    let port = config.port;

    let Some(database_url) = config.database_url.clone() else {
        tracing::error!("DATABASE_URL/POSTGRES_URL not set — truespotter-admin is a data service and cannot start");
        std::process::exit(1);
    };
    let db = match sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&database_url)
        .await
    {
        Ok(pool) => pool,
        Err(e) => {
            tracing::error!(error = %e, "Postgres pool creation failed");
            std::process::exit(1);
        }
    };
    if config.metrics_secret.is_none() || config.jwt_secret.is_none() {
        tracing::warn!(
            "METRICS_SECRET or JWT_SECRET not set — login will 500, guarded endpoints will 401/500"
        );
    }

    let state = Arc::new(AppState {
        db,
        metrics_secret: config.metrics_secret,
        jwt_secret: config.jwt_secret,
        opensearch_url: config.opensearch_url,
        opensearch_index: config.opensearch_index,
        opensearch_user: config.opensearch_user,
        opensearch_password: config.opensearch_password,
        http_client: reqwest::Client::new(),
    });
    let app = build_router(state);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("truespotter-admin listening on {}", addr);
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, port, "failed to bind listener");
            std::process::exit(1);
        }
    };
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!(error = %e, "server error");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use std::str::FromStr;
    use tower::util::ServiceExt;

    const METRICS_SECRET: &str = "test-metrics-secret";
    const JWT_SECRET: &str = "test-jwt-secret";

    fn test_state() -> Arc<AppState> {
        let db = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_millis(200))
            .connect_lazy_with(
                sqlx::postgres::PgConnectOptions::from_str("postgres://nobody@127.0.0.1:1/none")
                    .unwrap(),
            );
        Arc::new(AppState {
            db,
            metrics_secret: Some(METRICS_SECRET.to_string()),
            jwt_secret: Some(JWT_SECRET.to_string()),
            opensearch_url: "http://127.0.0.1:1".into(),
            opensearch_index: "products".into(),
            opensearch_user: None,
            opensearch_password: None,
            http_client: reqwest::Client::new(),
        })
    }

    async fn request(
        state: Arc<AppState>,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> axum::http::Response<axum::body::Body> {
        let mut req = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        build_router(state)
            .oneshot(req.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn body_str(resp: axum::http::Response<axum::body::Body>) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn health_ok() {
        let resp = request(test_state(), "GET", "/health", &[]).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_str(resp).await, r#"{"status":"ok"}"#);
    }

    #[tokio::test]
    async fn login_with_correct_secret_returns_token_and_expiry() {
        let resp = request(
            test_state(),
            "POST",
            "/api/admin/login",
            &[("x-admin-secret", METRICS_SECRET)],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_str(resp).await).unwrap();
        assert_eq!(body["expiresIn"], 3600);
        assert!(body["token"].as_str().unwrap().split('.').count() == 3);

        // The token validates against the guard: a guarded GET with it
        // passes auth (fails later at the DB, i.e. 500 — NOT 401).
        let token = body["token"].as_str().unwrap();
        let bearer = format!("Bearer {token}");
        let resp = request(
            test_state(),
            "GET",
            "/api/admin/chat/sessions?limit=5",
            &[("Authorization", &bearer)],
        )
        .await;
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn login_matrix_rejections() {
        // Wrong secret → 401 (empty body, like Results.Unauthorized()).
        let resp = request(
            test_state(),
            "POST",
            "/api/admin/login",
            &[("x-admin-secret", "wrong")],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(body_str(resp).await, "");
        // Missing header → 401.
        let resp = request(test_state(), "POST", "/api/admin/login", &[]).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // Empty header value → 401.
        let resp = request(
            test_state(),
            "POST",
            "/api/admin/login",
            &[("x-admin-secret", "")],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn guarded_endpoints_reject_anonymous_with_dotnet_401() {
        for (method, uri) in [
            ("GET", "/api/admin/chat/sessions"),
            ("GET", "/api/products"),
            ("GET", "/api/web-reviews"),
            ("GET", "/api/admin/stats"),
            ("GET", "/api/workflows"),
            ("GET", "/api/workflows/some-id"),
            ("GET", "/api/workflows/some-id/history"),
        ] {
            let resp = request(test_state(), method, uri, &[]).await;
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{method} {uri}");
            assert_eq!(body_str(resp).await, "Unauthorized");
        }
    }

    #[tokio::test]
    async fn x_admin_secret_passes_guard_on_every_get() {
        for (method, uri) in [
            ("GET", "/api/admin/chat/sessions?limit=1"),
            ("GET", "/api/web-reviews"),
            ("GET", "/api/workflows?pageSize=1"),
            ("GET", "/api/workflows/wf-1"),
            ("GET", "/api/workflows/wf-1/history"),
            ("GET", "/api/admin/stats"),
        ] {
            let resp = request(
                test_state(),
                method,
                uri,
                &[("x-admin-secret", METRICS_SECRET)],
            )
            .await;
            // Guard passed; the lazy DB (127.0.0.1:1) is unreachable → the
            // handler-level failure is 500 (stats still 200 — every count is
            // best-effort). NEVER 401.
            assert_ne!(resp.status(), StatusCode::UNAUTHORIZED, "{method} {uri}");
        }
        // stats never 500s — all counts degrade to 0.
        let resp = request(
            test_state(),
            "GET",
            "/api/admin/stats",
            &[("x-admin-secret", METRICS_SECRET)],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            body_str(resp).await,
            r#"{"productCount":0,"shopCount":0,"offerCount":0,"reviewCount":0,"chatSessionCount":0,"deviceCount":0,"failedWorkflowCount":0}"#
        );
    }

    #[tokio::test]
    async fn key_query_param_is_accepted_like_dotnet() {
        let uri = format!("/api/web-reviews?key={METRICS_SECRET}");
        let resp = request(test_state(), "GET", &uri, &[]).await;
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn visitor_jwt_without_admin_role_is_rejected() {
        // Zitadel-style ID token without the admin role claim: signed with
        // the same secret but no role → must NOT pass (privilege escalation
        // guard from AdminAuthExtensions).
        let claims = serde_json::json!({
            "sub": "11111111-2222-3333-4444-555555555555",
            "exp": (Utc::now() + chrono::Duration::hours(1)).timestamp(),
        });
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(JWT_SECRET.as_bytes()),
        )
        .unwrap();
        let bearer = format!("Bearer {token}");
        let resp = request(
            test_state(),
            "GET",
            "/api/web-reviews",
            &[("Authorization", &bearer)],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn admin_jwt_signed_with_wrong_secret_is_rejected() {
        let claims = serde_json::json!({
            "http://schemas.microsoft.com/ws/2008/06/identity/claims/role": "admin",
            "exp": (Utc::now() + chrono::Duration::hours(1)).timestamp(),
        });
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret("other".as_bytes()),
        )
        .unwrap();
        let bearer = format!("Bearer {token}");
        let resp = request(
            test_state(),
            "GET",
            "/api/web-reviews",
            &[("Authorization", &bearer)],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn missing_metrics_secret_yields_500_on_guard() {
        let mut state = AppState {
            db: sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(std::time::Duration::from_millis(200))
                .connect_lazy_with(
                    sqlx::postgres::PgConnectOptions::from_str(
                        "postgres://nobody@127.0.0.1:1/none",
                    )
                    .unwrap(),
                ),
            metrics_secret: None,
            jwt_secret: Some(JWT_SECRET.to_string()),
            opensearch_url: String::new(),
            opensearch_index: String::new(),
            opensearch_user: None,
            opensearch_password: None,
            http_client: reqwest::Client::new(),
        };
        state.metrics_secret = None;
        let resp = request(Arc::new(state), "GET", "/api/web-reviews", &[]).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body_str(resp).await, "METRICS_SECRET is not set");
    }

    #[tokio::test]
    async fn dotnet_time_format() {
        use chrono::TimeZone;
        let dt = Utc.timestamp_opt(1700000000, 138958400).unwrap();
        assert_eq!(format_dotnet_utc(&dt), "2023-11-14T22:13:20.1389584Z");
        let dt = Utc.timestamp_opt(1700000000, 0).unwrap();
        assert_eq!(format_dotnet_utc(&dt), "2023-11-14T22:13:20Z");
    }
}
