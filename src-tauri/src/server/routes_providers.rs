//! Providers 与配置档案(P0 profiles)路由。
//! 自 server.rs 原样迁出(仅可见性调整为 pub(crate)),行为零变化。

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Response,
    Json,
};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

use super::{claude_home, err_env, ok_env, val_errs_env, AppState};

// --- Providers（04 契约：统一信封 + 错误码）---

// GET /api/providers
pub(crate) async fn handle_providers_list(State(s): State<Arc<AppState>>) -> Response {
    let data = crate::providers::load(&s.providers_path);
    let providers: Vec<Value> = data
        .providers
        .iter()
        .map(crate::providers::public_provider)
        .collect();
    ok_env(json!({
        "providers": providers,
        "active_provider_id": data.active_provider_id,
        "active_provider_ids": data.active_provider_ids,
    }))
}

// POST /api/providers
pub(crate) async fn handle_providers_create(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let input = crate::providers::value_to_input(&body);
    match crate::providers::create(&s.providers_path, input) {
        Ok(p) => ok_env(crate::providers::public_provider(&p)),
        Err(errs) => val_errs_env(&errs),
    }
}

// PUT /api/providers/:id
pub(crate) async fn handle_providers_update(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    // PUT 可能来自旧版前端，仅提交了少量字段。数据层按字段保留未提交值，
    // 并在当前 Codex 托管该供应商时立即重应用 config/catalog，避免磁盘配置滞后。
    let hosted_codex = crate::desktop::detect_hosting(&s.config_path, &s.providers_path)
        .get("way")
        .and_then(Value::as_str)
        == Some("gateway");
    let active_codex = crate::providers::get_active_for_agent(&s.providers_path, "codex")
        .is_some_and(|provider| provider.id == id);
    let snapshot = if hosted_codex && active_codex {
        match crate::desktop::snapshot_file(&s.providers_path) {
            Ok(value) => Some(value),
            Err((_, _, message)) => {
                return err_env(StatusCode::INTERNAL_SERVER_ERROR, "E_IO", &message, None)
            }
        }
    } else {
        None
    };
    match crate::providers::update_from_value(&s.providers_path, &id, &body) {
        Ok(p) if hosted_codex && active_codex && p.agent == "codex" => {
            match crate::desktop::host(
                &s.config_path,
                &s.backup_dir,
                &s.codex_home,
                &s.providers_path,
                &id,
                "gateway",
            ) {
                Ok(_) => ok_env(crate::providers::public_provider(&p)),
                Err((status, code, message)) => {
                    if let Some(snapshot) = snapshot.as_ref() {
                        let _ = crate::desktop::restore_file_snapshot(&s.providers_path, snapshot);
                    }
                    err_env(
                        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
                        &code,
                        &message,
                        None,
                    )
                }
            }
        }
        Ok(p) => ok_env(crate::providers::public_provider(&p)),
        Err(errs) => {
            if errs.len() == 1 && errs[0].field == "id" {
                err_env(StatusCode::NOT_FOUND, "E_NOT_FOUND", "供应商不存在", None)
            } else {
                val_errs_env(&errs)
            }
        }
    }
}

// DELETE /api/providers/:id
pub(crate) async fn handle_providers_delete(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    // 持久化托管平台按「平台托管态 + 平台 active provider」双重判断，避免删除后磁盘仍引用旧供应商。
    // Claude Code 的托管快照位于 backup_dir，状态由配置托管模块统一判断。
    let hosted_agents = [
        (
            "codex",
            !crate::desktop::detect_hosting(&s.config_path, &s.providers_path).is_null(),
        ),
        (
            "hermes",
            !crate::agents::hermes::detect_state(&s.hermes_home.join("config.yaml"))["hosting"]
                .is_null(),
        ),
        (
            "gemini",
            !crate::agents::gemini::state(&s.gem_home)["hosting"].is_null(),
        ),
        (
            "claude",
            crate::agents::claude_code::state(
                claude_home(&s.codex_home),
                &s.backup_dir,
                &s.providers_path,
            )["hosted"]
                .as_bool()
                .unwrap_or(false),
        ),
        (
            "grokbuild",
            !crate::agents::grok::state(&s.grok_home)["hosting"].is_null(),
        ),
        (
            "opencode",
            !crate::agents::opencode::state(&s.oc_home)["hosting"].is_null(),
        ),
        (
            "openclaw",
            !crate::agents::openclaw::state(&s.oclaw_home)["hosting"].is_null(),
        ),
        (
            "claude-desktop",
            !crate::agents::claude_desktop::state(&s.cd_home)["hosting"].is_null(),
        ),
        (
            "workbuddy",
            !crate::agents::workbuddy::state(&s.wb_home)["hosting"].is_null(),
        ),
        (
            "cursor",
            !crate::agents::cursor::state(&s.cursor_home)["hosting"].is_null(),
        ),
    ];
    let hosted_by = hosted_agents.iter().find_map(|(agent, hosted)| {
        if *hosted
            && crate::providers::get_active_for_agent(&s.providers_path, agent)
                .is_some_and(|provider| provider.id == id)
        {
            Some(*agent)
        } else {
            None
        }
    });
    if let Some(agent) = hosted_by {
        return err_env(
            StatusCode::BAD_REQUEST,
            "E_PROVIDER_HOSTED",
            &format!("托管中的供应商不能删除,请先在「{agent}」还原官方"),
            None,
        );
    }
    match crate::providers::delete_checked(&s.providers_path, &id) {
        Ok(()) => ok_env(json!({ "id": id, "deleted": true })),
        Err(error) if error == "供应商不存在" => {
            err_env(StatusCode::NOT_FOUND, "E_NOT_FOUND", &error, None)
        }
        Err(error) => err_env(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_PROVIDER_WRITE",
            &error,
            None,
        ),
    }
}

// PUT /api/providers/reorder { ids }
pub(crate) async fn handle_providers_reorder(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let ids: Vec<String> = body
        .get("ids")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    match crate::providers::reorder_checked(&s.providers_path, &ids) {
        Ok(()) => ok_env(json!({ "reordered": true, "count": ids.len() })),
        Err(error) => err_env(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_PROVIDER_WRITE",
            &error,
            None,
        ),
    }
}

// GET /api/providers/active
pub(crate) async fn handle_providers_active(State(s): State<Arc<AppState>>) -> Response {
    match crate::providers::get_active(&s.providers_path) {
        Some(p) => ok_env(crate::providers::public_provider(&p)),
        None => ok_env(Value::Null),
    }
}

// POST /api/providers/activate { id }
pub(crate) async fn handle_providers_activate(
    State(_s): State<Arc<AppState>>,
    Json(_body): Json<Value>,
) -> Response {
    err_env(
        StatusCode::GONE,
        "E_CODEX_CONFIG_MUTATION_RETIRED",
        "Codex 旧版 activate 会改写官方凭据，已停用；请使用 /api/desktop/host 的 gateway 模式",
        None,
    )
}

// POST /api/providers/activate-official
pub(crate) async fn handle_providers_activate_official(
    State(_s): State<Arc<AppState>>,
) -> Response {
    err_env(
        StatusCode::GONE,
        "E_CODEX_CONFIG_MUTATION_RETIRED",
        "Codex 旧版 activate-official 会回灌 auth.json，已停用；请使用官方恢复预览与二次确认流程",
        None,
    )
}

// POST /api/providers/preview-config { id? 或临时 provider 对象 }
pub(crate) async fn handle_providers_preview(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let provider = if let Some(id) = body
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|x| !x.is_empty())
    {
        match crate::providers::load(&s.providers_path)
            .providers
            .into_iter()
            .find(|p| p.id == id)
        {
            Some(p) => p,
            None => return err_env(StatusCode::NOT_FOUND, "E_NOT_FOUND", "供应商不存在", None),
        }
    } else {
        crate::providers::input_to_provider(crate::providers::value_to_input(&body))
    };
    if provider.agent == "codex" {
        let fingerprint =
            crate::codex_overlay::fingerprint(&s.config_path).map_err(|error| error.to_string());
        return match fingerprint {
            Ok(fingerprint) => ok_env(json!({
                "mode": "gateway",
                "config": fingerprint,
                "auth_action": "noop",
                "auth_diff": Value::Null,
                "backup_will_create": false,
                "warning": "Codex 官方凭据由 CLI/keyring 管理，预览不会读取或改写 auth.json",
            })),
            Err(error) => err_env(
                StatusCode::BAD_REQUEST,
                "E_CODEX_CONFIG_PARSE",
                &error,
                None,
            ),
        };
    }
    match crate::config::preview_provider(&s.config_path, &s.codex_home, &provider) {
        Ok(o) => ok_env(json!({
            "config_toml": o.config_toml,
            "auth_action": o.auth_action,
            "auth_diff": o.auth_diff,
            "backup_will_create": o.backup_will_create,
        })),
        Err(e) => err_env(StatusCode::INTERNAL_SERVER_ERROR, "E_INTERNAL", &e, None),
    }
}

// POST /api/providers/fetch-models { id? 或 baseUrl+apiKey（新建未保存时也能拉）}
pub(crate) async fn handle_providers_fetch_models(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let id = body.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let (base_url, api_key, write_back_id): (String, String, Option<String>) = if !id.is_empty() {
        let data = crate::providers::load(&s.providers_path);
        match data.providers.iter().find(|p| p.id == id).cloned() {
            Some(mut p) => {
                // 改 Key 后按新 Key 拉取:body.apiKey(未保存的新 Key)若提供,覆盖存储 Key 用于本次探测
                // (不落盘——保存动作走 PUT /api/providers/:id;前端编辑表单改 Key 后先拉模型再保存)。
                let k = body
                    .get("apiKey")
                    .or_else(|| body.get("api_key"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                if !k.is_empty() {
                    p.api_key = k.to_string();
                }
                (p.base_url, p.api_key, Some(id.to_string()))
            }
            None => return err_env(StatusCode::NOT_FOUND, "E_NOT_FOUND", "供应商不存在", None),
        }
    } else {
        let b = body
            .get("baseUrl")
            .or_else(|| body.get("base_url"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let k = body
            .get("apiKey")
            .or_else(|| body.get("api_key"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        (b, k, None)
    };
    if base_url.trim().is_empty() || api_key.trim().is_empty() {
        return err_env(
            StatusCode::BAD_REQUEST,
            "E_BAD_REQUEST",
            "需要 baseUrl + apiKey",
            None,
        );
    }
    let probed = crate::probe::probe_endpoint(&base_url, &api_key).await;
    let models: Vec<crate::providers::ModelConfig> = probed
        .iter()
        .map(|(n, ctx)| crate::providers::ModelConfig {
            name: n.clone(),
            context_window: *ctx,
            ..Default::default()
        })
        .collect();
    // reasoning levels 探测已移出同步路径(2026-08-15 真机:2xa 上游对该探测挂满 15s 超时,
    // 把拉模型拖到 25s+,用户感知"拉取用不了")。levels 为空时 catalog 用默认 5 级,
    // 真机对话已验证无影响;显式探测挪到阶段 2 preflight。写回仅更新 models,保留已存 levels。
    let levels: Vec<String> = if let Some(wid) = &write_back_id {
        crate::providers::load(&s.providers_path)
            .providers
            .iter()
            .find(|p| p.id == *wid)
            .and_then(|p| p.reasoning_levels.clone())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    if let Some(wid) = write_back_id {
        // 探测失败(网络/上游异常)返回空列表时不落盘清空已存模型表,保留原值
        if !models.is_empty() {
            let mut data = crate::providers::load(&s.providers_path);
            if let Some(p) = data.providers.iter_mut().find(|p| p.id == wid) {
                p.models = models.clone();
            }
            let _ = crate::providers::store(&s.providers_path, &data);
        }
    }
    ok_env(json!({ "models": models, "reasoning_levels": levels }))
}

// POST /api/providers/fetch-balance（01-D6 stub）
pub(crate) async fn handle_providers_fetch_balance() -> Response {
    ok_env(json!({ "balance": Value::Null, "note": "stub" }))
}

// POST /api/providers/diagnose { id }
pub(crate) async fn handle_providers_diagnose(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let id = body.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let data = crate::providers::load(&s.providers_path);
    let provider = match data.providers.iter().find(|p| p.id == id).cloned() {
        Some(p) => p,
        None => return err_env(StatusCode::NOT_FOUND, "E_NOT_FOUND", "供应商不存在", None),
    };
    let result = crate::diagnose::diagnose(&provider).await;
    ok_env(serde_json::to_value(&result).unwrap_or(json!({})))
}

// ── P0 配置档案 ─────────────────────────────────────────────

pub(crate) async fn handle_profiles_list(
    State(s): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let agent = query.get("agent").map(String::as_str);
    let data = crate::profiles::list(&s.codex_home.join("2xapi-profiles.json"), agent);
    ok_env(json!({
        "profiles": data.profiles,
        "activeProfiles": data.active_profiles,
        "activeProfileId": data.active_profiles.get(agent.unwrap_or("codex")),
    }))
}

pub(crate) async fn handle_profiles_create(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let provider_id = body
        .get("providerId")
        .or_else(|| body.get("provider_id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if !crate::providers::load(&s.providers_path)
        .providers
        .iter()
        .any(|provider| provider.id == provider_id)
    {
        return err_env(
            StatusCode::UNPROCESSABLE_ENTITY,
            "E_PROFILE_PROVIDER_NOT_FOUND",
            "profile 引用的供应商不存在",
            None,
        );
    }
    match crate::profiles::create(&s.codex_home.join("2xapi-profiles.json"), &body) {
        Ok(profile) => ok_env(json!({ "profile": profile })),
        Err(error) => err_env(StatusCode::UNPROCESSABLE_ENTITY, "E_PROFILE", &error, None),
    }
}

pub(crate) async fn handle_profiles_update(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let provider_id = body
        .get("providerId")
        .or_else(|| body.get("provider_id"))
        .and_then(Value::as_str);
    if let Some(provider_id) = provider_id {
        if !crate::providers::load(&s.providers_path)
            .providers
            .iter()
            .any(|provider| provider.id == provider_id)
        {
            return err_env(
                StatusCode::UNPROCESSABLE_ENTITY,
                "E_PROFILE_PROVIDER_NOT_FOUND",
                "profile 引用的供应商不存在",
                None,
            );
        }
    }
    match crate::profiles::update(&s.codex_home.join("2xapi-profiles.json"), &id, &body) {
        Ok(profile) => ok_env(json!({ "profile": profile })),
        Err(error) => err_env(StatusCode::UNPROCESSABLE_ENTITY, "E_PROFILE", &error, None),
    }
}

pub(crate) async fn handle_profiles_delete(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    match crate::profiles::delete(&s.codex_home.join("2xapi-profiles.json"), &id) {
        Ok(()) => ok_env(json!({ "deleted": true })),
        Err(error) => err_env(StatusCode::NOT_FOUND, "E_PROFILE_NOT_FOUND", &error, None),
    }
}

pub(crate) async fn handle_profiles_preview(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    match crate::profiles::preview(&s.codex_home, &s.config_path, &s.providers_path, &body) {
        Ok(result) => ok_env(result),
        Err(error) => err_env(
            StatusCode::UNPROCESSABLE_ENTITY,
            "E_PROFILE_PREVIEW",
            &error,
            None,
        ),
    }
}

pub(crate) async fn handle_profiles_apply(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let id = body.get("id").and_then(Value::as_str).unwrap_or("").trim();
    let token = body
        .get("previewToken")
        .or_else(|| body.get("preview_token"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let confirmed = body
        .get("confirmed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match crate::profiles::apply(
        &s.codex_home,
        &s.config_path,
        &s.backup_dir,
        &s.providers_path,
        id,
        token,
        confirmed,
    ) {
        Ok(result) => ok_env(result),
        Err((status, code, message)) => err_env(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
            &code,
            &message,
            None,
        ),
    }
}
