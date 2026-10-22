//! GET /api/admin/chat/sessions?limit= — port of the admin listing in
//! ChatSessionEndpoints.cs + ChatRepository.ListAllSessionsAsync (the
//! paged variant, page 1, joined with user_profiles — same SQL as
//! truespotter-core sessions.rs).

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use sqlx::postgres::PgRow;
use sqlx::Row;
use std::sync::Arc;
use uuid::Uuid;

use crate::{AppState, DotNetTime};

/// AdminChatSessionDto (IChatRepository.cs) — camelCase, nulls NOT omitted.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminSessionDto {
    pub id: String,
    pub visitor_id: String,
    pub title: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub preferences: Option<String>,
    pub profile_total_messages: i64,
    pub last_active: Option<DotNetTime>,
    pub created_at: DotNetTime,
    pub updated_at: DotNetTime,
    pub session_message_count: i32,
    pub deleted_at: Option<DotNetTime>,
}

impl AdminSessionDto {
    fn from_row(row: &PgRow) -> Self {
        Self {
            id: row.get::<Uuid, _>("id").to_string(),
            visitor_id: row.get::<Uuid, _>("visitor_id").to_string(),
            title: row.get("title"),
            display_name: row.get("display_name"),
            email: row.get("user_email"),
            preferences: row.get("preferences"),
            profile_total_messages: row
                .get::<Option<i32>, _>("profile_total_messages")
                .unwrap_or(0) as i64,
            last_active: row
                .get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_active")
                .map(DotNetTime),
            created_at: DotNetTime(row.get("created_at")),
            updated_at: DotNetTime(row.get("updated_at")),
            session_message_count: row.get("session_message_count"),
            deleted_at: row
                .get::<Option<chrono::DateTime<chrono::Utc>>, _>("deleted_at")
                .map(DotNetTime),
        }
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct SessionsQuery {
    pub limit: Option<i32>,
}

/// ChatRepository.ListAllSessionsAsync(limit) — page 1 with pageSize=limit.
const SELECT: &str = "SELECT s.id, s.visitor_id AS visitor_id, s.title, \
    p.display_name, p.email AS user_email, \
    p.preferences::text AS preferences, \
    p.total_messages AS profile_total_messages, p.last_active AS last_active, \
    s.created_at, s.updated_at, s.deleted_at, \
    (SELECT COUNT(*)::int FROM chat_messages WHERE session_id = s.id) AS session_message_count \
    FROM chat_sessions s \
    LEFT JOIN user_profiles p ON s.visitor_id = p.visitor_id \
    ORDER BY s.updated_at DESC LIMIT $1 OFFSET $2";

pub async fn list_all_sessions(
    db: &sqlx::PgPool,
    limit: i64,
) -> sqlx::Result<Vec<AdminSessionDto>> {
    let rows = sqlx::query(SELECT)
        .bind(limit)
        .bind(0i64)
        .fetch_all(db)
        .await?;
    Ok(rows.iter().map(AdminSessionDto::from_row).collect())
}

pub async fn list_sessions_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SessionsQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(200).clamp(1, i32::MAX) as i64;
    match list_all_sessions(&state.db, limit).await {
        Ok(items) => Json(items).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "list_all_sessions failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dto_keys_match_adminservice_adminsession() {
        // AdminService.ts AdminSession consumes (subset): id, visitorId,
        // title, displayName, email, profileTotalMessages, lastActive,
        // createdAt, updatedAt, sessionMessageCount (+ preferences/deletedAt
        // from the .NET AdminChatSessionDto — full wire contract).
        let dto = AdminSessionDto {
            id: "id".into(),
            visitor_id: "v".into(),
            title: "t".into(),
            display_name: None,
            email: None,
            preferences: None,
            profile_total_messages: 0,
            last_active: None,
            created_at: DotNetTime::default(),
            updated_at: DotNetTime::default(),
            session_message_count: 0,
            deleted_at: None,
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
                "visitorId",
                "title",
                "displayName",
                "email",
                "preferences",
                "profileTotalMessages",
                "lastActive",
                "createdAt",
                "updatedAt",
                "sessionMessageCount",
                "deletedAt",
            ]
        );
    }
}
