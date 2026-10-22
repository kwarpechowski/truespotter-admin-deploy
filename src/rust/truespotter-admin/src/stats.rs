//! GET /api/admin/stats — port of AdminStatsEndpoints.cs. Every count is
//! best-effort (telemetry-never-throws): a failing source yields 0, the
//! endpoint always returns 200 with the AdminStatsDto camelCase shape.

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use std::sync::Arc;

use crate::AppState;

/// AdminStatsDto (AdminStatsEndpoints.cs) — camelCase.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminStatsDto {
    pub product_count: i64,
    pub shop_count: i64,
    pub offer_count: i64,
    pub review_count: i64,
    pub chat_session_count: i64,
    pub device_count: i64,
    pub failed_workflow_count: i64,
}

async fn count(db: &sqlx::PgPool, sql: &str) -> i64 {
    match sqlx::query_scalar::<_, i64>(sql).fetch_one(db).await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, sql = sql, "stats count failed — yielding 0");
            0
        }
    }
}

/// products/_count on OpenSearch (catalog is the source of truth).
async fn opensearch_count(state: &AppState) -> i64 {
    let url = format!(
        "{}/{}/_count",
        state.opensearch_url.trim_end_matches('/'),
        state.opensearch_index
    );
    let mut req = state.http_client.get(&url);
    if let (Some(user), Some(password)) = (&state.opensearch_user, &state.opensearch_password) {
        req = req.basic_auth(user, Some(password));
    }
    match req.send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<serde_json::Value>().await {
            Ok(v) => v["count"].as_i64().unwrap_or(0),
            Err(e) => {
                tracing::warn!(error = %e, "opensearch count body parse failed");
                0
            }
        },
        Ok(resp) => {
            tracing::warn!(status = %resp.status(), "opensearch count failed — yielding 0");
            0
        }
        Err(e) => {
            tracing::warn!(error = %e, "opensearch count request failed — yielding 0");
            0
        }
    }
}

/// Failed workflows among the 100 most recent (closed, status failed=2).
/// .NET counts `Status contains "fail"/"error"` over the last 100 started
/// workflows (in-process Temporal _recent registry) — SQL-visibility
/// approximation, same 100-row window.
async fn failed_workflow_count(db: &sqlx::PgPool) -> i64 {
    match sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM (SELECT status FROM temporal_visibility.closed_executions \
         ORDER BY start_time DESC LIMIT 100) recent WHERE status = 2",
    )
    .fetch_one(db)
    .await
    {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, "failed workflow count failed — yielding 0");
            0
        }
    }
}

pub async fn stats(state: &AppState) -> AdminStatsDto {
    // Independent counts — every failure is contained to its own number.
    let products = opensearch_count(state).await;
    let shops = count(&state.db, "SELECT COUNT(*) FROM shops").await;
    let offers = count(&state.db, "SELECT COUNT(*) FROM offers").await;
    let web_reviews = count(&state.db, "SELECT COUNT(*) FROM web_reviews").await;
    let youtube_reviews = count(&state.db, "SELECT COUNT(*) FROM youtube_reviews").await;
    let chat_sessions = count(&state.db, "SELECT COUNT(*) FROM chat_sessions").await;
    let devices = count(&state.db, "SELECT COUNT(*) FROM device_registrations").await;
    let failed_workflows = failed_workflow_count(&state.db).await;

    AdminStatsDto {
        product_count: products,
        shop_count: shops,
        offer_count: offers,
        review_count: web_reviews + youtube_reviews,
        chat_session_count: chat_sessions,
        device_count: devices,
        failed_workflow_count: failed_workflows,
    }
}

pub async fn stats_handler(State(state): State<Arc<AppState>>) -> Response {
    Json(stats(&state).await).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dto_keys_match_adminservice_adminstats() {
        // AdminService.ts AdminStats — exact key set and order.
        let dto = AdminStatsDto {
            product_count: 0,
            shop_count: 0,
            offer_count: 0,
            review_count: 0,
            chat_session_count: 0,
            device_count: 0,
            failed_workflow_count: 0,
        };
        let json = serde_json::to_value(&dto).unwrap();
        let keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(
            keys,
            vec![
                "productCount",
                "shopCount",
                "offerCount",
                "reviewCount",
                "chatSessionCount",
                "deviceCount",
                "failedWorkflowCount",
            ]
        );
    }
}
