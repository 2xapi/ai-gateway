//! 历史会话管理路由(列表/检查/修复任务/删除/恢复/设置)。
//! 自 server.rs 原样迁出(仅可见性调整为 pub(crate)),行为零变化。

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Response,
    Json,
};
use serde_json::{json, Value};
use std::sync::Arc;

use super::{err_env, ok_env, AppState};

// ── 历史会话管理(阶段 3,任务书 §四)─────────────────────

// GET /api/sessions?page=&size= → Codex++ 风格分页、统计和数据库路径
pub(crate) async fn handle_sessions_list(
    State(s): State<Arc<AppState>>,
    query: axum::extract::Query<Value>,
) -> Response {
    let page = query
        .get("page")
        .and_then(|v| v.as_str())
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1);
    let size = query
        .get("size")
        .and_then(|v| v.as_str())
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(50);
    let snapshot_id = query
        .get("snapshotId")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let data = match snapshot_id {
        Some(snapshot_id) => match crate::sessions::list_sessions_page_from_snapshot(
            &s.codex_home,
            snapshot_id,
            page,
            size,
        ) {
            Ok(data) => data,
            Err(error) => return err_env(StatusCode::CONFLICT, "E_SESSION_SNAPSHOT", &error, None),
        },
        None => {
            return err_env(
                StatusCode::CONFLICT,
                "E_SESSION_SNAPSHOT",
                "请先完成会话检查，再加载列表",
                None,
            )
        }
    };
    ok_env(data)
}

pub(crate) async fn handle_sessions_inspect(State(s): State<Arc<AppState>>) -> Response {
    ok_env(crate::sessions::inspect_sessions(&s.codex_home))
}

pub(crate) async fn handle_sessions_repair_preview(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let target = body
        .get("targetProvider")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    match crate::sessions::preview_repair(&s.codex_home, target) {
        Ok(data) => ok_env(data),
        Err(error) => err_env(
            StatusCode::BAD_REQUEST,
            "E_SESSION_REPAIR_PREVIEW",
            &error,
            None,
        ),
    }
}

pub(crate) async fn handle_sessions_repair(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let target = body
        .get("targetProvider")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if let Some(token) = body.get("previewToken").and_then(Value::as_str) {
        if let Err(error) = crate::sessions::validate_repair_preview(target, token) {
            return err_env(
                StatusCode::CONFLICT,
                "E_SESSION_REPAIR_PREVIEW",
                &error,
                None,
            );
        }
    }
    match crate::sessions::start_repair_job(
        s.codex_home.clone(),
        s.backup_dir.clone(),
        target.to_string(),
    ) {
        Ok(job_id) => ok_env(json!({"jobId": job_id})),
        Err(error) => err_env(
            if error.contains("正在运行") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            },
            "E_SESSION_REPAIR",
            &error,
            None,
        ),
    }
}

pub(crate) async fn handle_sessions_job(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    match crate::sessions::get_repair_job_for_home(&s.codex_home, &id) {
        Some(job) => ok_env(job),
        None => err_env(
            StatusCode::NOT_FOUND,
            "E_SESSION_JOB",
            "修复任务不存在",
            None,
        ),
    }
}

pub(crate) async fn handle_sessions_job_cancel(Path(id): Path<String>) -> Response {
    match crate::sessions::cancel_repair_job(&id) {
        Ok(job) => ok_env(job),
        Err(error) => err_env(StatusCode::CONFLICT, "E_SESSION_JOB_CANCEL", &error, None),
    }
}

pub(crate) async fn handle_sessions_job_resume(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    // 应用重启后中断任务只存在于磁盘;先按 codex_home 补载进任务表,恢复才能找到检查点。
    crate::sessions::get_repair_job_for_home(&s.codex_home, &id);
    match crate::sessions::resume_repair_job(&id) {
        Ok(job_id) => ok_env(json!({"jobId": job_id, "resumedFrom": id})),
        Err(error) => err_env(StatusCode::CONFLICT, "E_SESSION_JOB_RESUME", &error, None),
    }
}

pub(crate) async fn handle_sessions_delete_preview(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let ids: Vec<String> = body
        .get("ids")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    match crate::sessions::preview_delete(&s.codex_home, &ids) {
        Ok(data) => ok_env(data),
        Err(error) => err_env(
            StatusCode::BAD_REQUEST,
            "E_SESSION_DELETE_PREVIEW",
            &error,
            None,
        ),
    }
}

pub(crate) async fn handle_sessions_delete(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let token = body
        .get("confirmToken")
        .and_then(Value::as_str)
        .unwrap_or("");
    match crate::sessions::apply_delete(&s.codex_home, &s.backup_dir, token) {
        Ok(data) => ok_env(data),
        Err(error) => err_env(StatusCode::BAD_REQUEST, "E_SESSION_DELETE", &error, None),
    }
}

pub(crate) async fn handle_sessions_delete_undo(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let backup = body.get("backupId").and_then(Value::as_str).unwrap_or("");
    match crate::sessions::undo_delete(&s.codex_home, &s.backup_dir, backup) {
        Ok(data) => ok_env(data),
        Err(error) => err_env(StatusCode::BAD_REQUEST, "E_SESSION_UNDO", &error, None),
    }
}

pub(crate) async fn handle_sessions_restart_codex(State(s): State<Arc<AppState>>) -> Response {
    if let Err(error) = crate::sessions::auto_repair_before_launch(&s.codex_home, &s.backup_dir) {
        return err_env(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_SESSION_AUTO_REPAIR",
            &error,
            None,
        );
    }
    match crate::sessions::restart_codex() {
        Ok(data) => ok_env(data),
        Err(error) => err_env(StatusCode::BAD_REQUEST, "E_SESSION_RESTART", &error, None),
    }
}

// POST /api/sessions/:id/resume → 在系统 Terminal 打开固定 codex resume 命令
pub(crate) async fn handle_sessions_resume(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    if let Err(error) = crate::sessions::auto_repair_before_launch(&s.codex_home, &s.backup_dir) {
        return err_env(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_SESSION_AUTO_REPAIR",
            &error,
            None,
        );
    }
    match crate::sessions::resume_session(&s.codex_home, &id) {
        Ok(data) => ok_env(data),
        Err(error) => err_env(StatusCode::BAD_REQUEST, "E_SESSION_RESUME", &error, None),
    }
}

// GET/POST /api/sessions/settings
pub(crate) async fn handle_sessions_settings(State(s): State<Arc<AppState>>) -> Response {
    ok_env(crate::sessions::get_settings(&s.codex_home))
}
pub(crate) async fn handle_sessions_settings_set(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let Some(value) = body.get("autoRepairBeforeLaunch").and_then(Value::as_bool) else {
        return err_env(
            StatusCode::UNPROCESSABLE_ENTITY,
            "E_VALIDATION",
            "autoRepairBeforeLaunch 必须是 boolean",
            Some(vec!["autoRepairBeforeLaunch".into()]),
        );
    };
    match crate::sessions::set_settings(&s.codex_home, value) {
        Ok(data) => ok_env(data),
        Err(error) => err_env(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_SESSION_SETTINGS",
            &error,
            None,
        ),
    }
}
