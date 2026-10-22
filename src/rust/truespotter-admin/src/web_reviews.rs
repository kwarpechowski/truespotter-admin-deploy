//! GET /api/web-reviews — port of the WebReviewEndpoints.cs LIST handler:
//! all rows ordered by CreatedAt DESC, WebReviewDto shape (camelCase).

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use sqlx::Row;
use std::sync::Arc;
use uuid::Uuid;

use crate::{AppState, DotNetTime};

/// WebReviewDto (WebReviewEndpoints.cs) — wire order preserved.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebReviewDto {
    pub id: String,
    pub source_url: String,
    pub source_name: String,
    pub title: String,
    pub status: String,
    pub review_text: String,
    pub matched_count: i32,
    pub error_message: Option<String>,
    pub discovery_mode: String,
    pub created_at: DotNetTime,
    pub updated_at: DotNetTime,
}

pub async fn list_web_reviews(db: &sqlx::PgPool) -> sqlx::Result<Vec<WebReviewDto>> {
    let rows = sqlx::query(
        "SELECT id, source_url, source_name, title, status, review_text, \
         matched_count, error_message, discovery_mode, created_at, updated_at \
         FROM web_reviews ORDER BY created_at DESC",
    )
    .fetch_all(db)
    .await?;

    Ok(rows
        .iter()
        .map(|row| WebReviewDto {
            id: row.get::<Uuid, _>("id").to_string(),
            source_url: row.get("source_url"),
            source_name: row.get("source_name"),
            title: row.get("title"),
            status: row.get("status"),
            review_text: row.get("review_text"),
            matched_count: row.get("matched_count"),
            error_message: row.get("error_message"),
            discovery_mode: row.get("discovery_mode"),
            created_at: DotNetTime(row.get("created_at")),
            updated_at: DotNetTime(row.get("updated_at")),
        })
        .collect())
}

pub async fn list_web_reviews_handler(State(state): State<Arc<AppState>>) -> Response {
    match list_web_reviews(&state.db).await {
        Ok(items) => Json(items).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "list_web_reviews failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dto_keys_match_adminservice_adminwebreview() {
        // AdminService.ts AdminWebReview consumes: id, sourceUrl, sourceName,
        // title, status, matchedCount, createdAt — the full .NET WebReviewDto
        // also carries reviewText, errorMessage, discoveryMode, updatedAt.
        let dto = WebReviewDto {
            id: "id".into(),
            source_url: "https://x".into(),
            source_name: "x".into(),
            title: "t".into(),
            status: "completed".into(),
            review_text: "rt".into(),
            matched_count: 0,
            error_message: None,
            discovery_mode: "manual".into(),
            created_at: DotNetTime::default(),
            updated_at: DotNetTime::default(),
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
                "id",
                "sourceUrl",
                "sourceName",
                "title",
                "status",
                "reviewText",
                "matchedCount",
                "errorMessage",
                "discoveryMode",
                "createdAt",
                "updatedAt",
            ]
        );
    }
}
