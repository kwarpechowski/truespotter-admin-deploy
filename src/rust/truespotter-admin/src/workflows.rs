//! GET /api/workflows, /api/workflows/{id}, /api/workflows/{id}/history —
//! port of the WorkflowEndpoints.cs READ handlers. .NET goes through the
//! Temporal gRPC client (+ an in-process _recent registry); this service
//! reads the Temporal SQL visibility tables that live in the SAME Postgres
//! (auto-setup creates schema `temporal_visibility` with
//! `open_executions` / `closed_executions`). Wire contract: WorkflowStatusDto
//! (camelCase: id, status, result, error, workflowType, startTime) and
//! WorkflowLogEntry (camelCase: eventType, message, timestamp).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use sqlx::Row;
use std::sync::Arc;

use crate::{AppState, DotNetTime};

/// WorkflowStatusDto (IProductRepository.cs).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowStatusDto {
    pub id: String,
    pub status: String,
    pub result: Option<String>,
    pub error: Option<String>,
    pub workflow_type: Option<String>,
    pub start_time: Option<DotNetTime>,
}

/// WorkflowLogEntry (IProductRepository.cs).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowLogEntry {
    pub event_type: String,
    pub message: String,
    pub timestamp: DotNetTime,
}

#[derive(Debug, serde::Deserialize)]
pub struct ListQuery {
    pub page_size: Option<i64>,
}

/// Temporal WorkflowExecutionStatus → the Temporalio enum ToString the .NET
/// GetStatusAsync returns (closed_executions.status: 1..6; open = Running).
pub fn status_name(is_open: bool, closed_status: Option<i32>) -> &'static str {
    if is_open {
        return "Running";
    }
    match closed_status {
        Some(1) => "Completed",
        Some(2) => "Failed",
        Some(3) => "Canceled",
        Some(4) => "Terminated",
        Some(5) => "ContinuedAsNew",
        Some(6) => "TimedOut",
        _ => "Unknown",
    }
}

struct VisibilityRow {
    workflow_id: String,
    workflow_type_name: Option<String>,
    start_time: chrono::DateTime<chrono::Utc>,
    close_time: Option<chrono::DateTime<chrono::Utc>>,
    status: Option<i32>,
}

/// Fetch one workflow from the visibility tables (closed first, then open).
async fn fetch_workflow(db: &sqlx::PgPool, id: &str) -> sqlx::Result<Option<VisibilityRow>> {
    if let Ok(row) = sqlx::query(
        "SELECT workflow_id, workflow_type_name, start_time, close_time, status \
         FROM temporal_visibility.closed_executions WHERE workflow_id = $1 \
         ORDER BY start_time DESC LIMIT 1",
    )
    .bind(id)
    .fetch_one(db)
    .await
    {
        return Ok(Some(VisibilityRow {
            workflow_id: row.get("workflow_id"),
            workflow_type_name: row.get("workflow_type_name"),
            start_time: row.get("start_time"),
            close_time: row.get("close_time"),
            status: Some(row.get::<i32, _>("status")),
        }));
    }
    if let Ok(row) = sqlx::query(
        "SELECT workflow_id, workflow_type_name, start_time \
         FROM temporal_visibility.open_executions WHERE workflow_id = $1 \
         ORDER BY start_time DESC LIMIT 1",
    )
    .bind(id)
    .fetch_one(db)
    .await
    {
        return Ok(Some(VisibilityRow {
            workflow_id: row.get("workflow_id"),
            workflow_type_name: row.get("workflow_type_name"),
            start_time: row.get("start_time"),
            close_time: None,
            status: None,
        }));
    }
    Ok(None)
}

fn to_status_dto(row: &VisibilityRow) -> WorkflowStatusDto {
    let status = status_name(row.close_time.is_none(), row.status);
    let (result, error) = match status {
        "Completed" => (Some("completed".to_string()), None),
        "Failed" | "TimedOut" => (None, Some("Workflow failed".to_string())),
        _ => (None, None),
    };
    WorkflowStatusDto {
        id: row.workflow_id.clone(),
        status: status.to_string(),
        result,
        error,
        workflow_type: row.workflow_type_name.clone(),
        start_time: Some(DotNetTime(row.start_time)),
    }
}

/// GET /api/workflows?pageSize=20 — recent workflows, newest first.
/// .NET returns [] on any Temporal failure (never 500) — mirrored here.
pub async fn list_workflows_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ListQuery>,
) -> Response {
    let page_size = q.page_size.unwrap_or(20).clamp(1, 1000);
    let rows = match sqlx::query(
        "SELECT workflow_id, workflow_type_name, start_time, close_time, status \
         FROM temporal_visibility.closed_executions \
         ORDER BY start_time DESC LIMIT $1",
    )
    .bind(page_size)
    .fetch_all(&state.db)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "workflow list failed — returning empty list");
            return Json(Vec::<WorkflowStatusDto>::new()).into_response();
        }
    };
    let items: Vec<WorkflowStatusDto> = rows
        .iter()
        .map(|row| {
            to_status_dto(&VisibilityRow {
                workflow_id: row.get("workflow_id"),
                workflow_type_name: row.get("workflow_type_name"),
                start_time: row.get("start_time"),
                close_time: row.get("close_time"),
                status: Some(row.get::<i32, _>("status")),
            })
        })
        .collect();
    Json(items).into_response()
}

/// GET /api/workflows/{id} — 404 on unknown id (.NET catch → NotFound).
pub async fn get_workflow_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    match fetch_workflow(&state.db, &id).await {
        Ok(Some(row)) => Json(to_status_dto(&row)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::warn!(error = %e, workflow = %id, "workflow status lookup failed");
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

/// GET /api/workflows/{id}/history — JSON event history.
///
/// Deviation: .NET streams the full Temporal event history over gRPC
/// (activity scheduling/running/completed events, Polish messages). The
/// SQL-visibility port synthesizes the terminal lifecycle events with the
/// same .NET event types and Polish messages; per-activity events are not
/// available in the visibility tables.
pub async fn workflow_history_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let row = match fetch_workflow(&state.db, &id).await {
        Ok(Some(row)) => row,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::warn!(error = %e, workflow = %id, "workflow history lookup failed");
            return StatusCode::NOT_FOUND.into_response();
        }
    };

    let mut entries = vec![WorkflowLogEntry {
        event_type: "WorkflowExecutionStarted".into(),
        message: "Workflow uruchomiony".into(),
        timestamp: DotNetTime(row.start_time),
    }];
    match status_name(row.close_time.is_none(), row.status) {
        "Completed" => entries.push(WorkflowLogEntry {
            event_type: "WorkflowExecutionCompleted".into(),
            message: "Workflow zakończony pomyślnie".into(),
            timestamp: DotNetTime(row.close_time.unwrap_or(row.start_time)),
        }),
        "Failed" => entries.push(WorkflowLogEntry {
            event_type: "WorkflowExecutionFailed".into(),
            message: "Workflow zakończony błędem: Workflow failed".into(),
            timestamp: DotNetTime(row.close_time.unwrap_or(row.start_time)),
        }),
        "Canceled" => entries.push(WorkflowLogEntry {
            event_type: "WorkflowExecutionCanceled".into(),
            message: "Workflow anulowany".into(),
            timestamp: DotNetTime(row.close_time.unwrap_or(row.start_time)),
        }),
        "Terminated" => entries.push(WorkflowLogEntry {
            event_type: "WorkflowExecutionTerminated".into(),
            message: "Workflow zatrzymany".into(),
            timestamp: DotNetTime(row.close_time.unwrap_or(row.start_time)),
        }),
        _ => {} // Running — the start event is all visibility knows.
    }
    Json(entries).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_names_match_temporalio_enum() {
        assert_eq!(status_name(true, None), "Running");
        assert_eq!(status_name(false, Some(1)), "Completed");
        assert_eq!(status_name(false, Some(2)), "Failed");
        assert_eq!(status_name(false, Some(3)), "Canceled");
        assert_eq!(status_name(false, Some(4)), "Terminated");
        assert_eq!(status_name(false, Some(5)), "ContinuedAsNew");
        assert_eq!(status_name(false, Some(6)), "TimedOut");
        assert_eq!(status_name(false, None), "Unknown");
    }

    #[test]
    fn dto_keys_match_adminservice_types() {
        // AdminService.ts AdminWorkflow: id, status, result, error,
        // workflowType, startTime.
        let dto = WorkflowStatusDto {
            id: "wf".into(),
            status: "Running".into(),
            result: None,
            error: None,
            workflow_type: Some("ImportProductsWorkflow".into()),
            start_time: None,
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
                "status",
                "result",
                "error",
                "workflowType",
                "startTime"
            ]
        );

        // AdminService.ts WorkflowHistoryEntry: eventType, message, timestamp.
        let entry = WorkflowLogEntry {
            event_type: "WorkflowExecutionStarted".into(),
            message: "Workflow uruchomiony".into(),
            timestamp: DotNetTime::default(),
        };
        let json = serde_json::to_value(&entry).unwrap();
        let keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(keys, vec!["eventType", "message", "timestamp"]);
    }
}
