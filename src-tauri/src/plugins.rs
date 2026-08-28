//! http 型插件接线(超融合 A 线二期,媒体组 M3 契约 v1)。
//!
//! - 登记:`POST /api/plugins {endpoint}` → GET {endpoint}/manifest 校验 → registry plugin 条目
//! - 调用:`POST /api/plugins/:id/invoke {op,…}` → POST {endpoint}/invoke,按 manifest.timeout_ms 断流;
//!   插件侧失败也走 200 包错误({ok:false,error:{code,message,human}})→ 透传;
//!   5xx/超时 → 网关侧人话 E_MEDIA_PLUGIN_DOWN / E_MEDIA_PLUGIN_TIMEOUT(媒体关卡人话原则)
//! - 四挂载点声明永久冻结:media_parse | tool_exec | proto_convert | dispatch

use crate::registry;
use crate::server::AppState;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Map, Value};
use std::{net::IpAddr, sync::Arc};

pub const MOUNTS: &[&str] = &["media_parse", "tool_exec", "proto_convert", "dispatch"];
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

fn err_env(status: StatusCode, code: &str, msg: &str) -> Response {
    crate::server::err_env(status, code, msg, None)
}

/// 透传插件原始 JSON(不套统一信封——invoke 契约是插件响应原样到达调用方)。
fn raw_json(status: StatusCode, v: &Value) -> Response {
    (status, axum::Json(v.clone())).into_response()
}

fn plugin_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap_or_default()
}

fn blocked_plugin_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(address) => {
            !(address.is_loopback())
                && (address.is_private()
                    || address.is_link_local()
                    || address.is_unspecified()
                    || address.is_multicast()
                    || address.octets() == [169, 254, 169, 254])
        }
        IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4_mapped() {
                return blocked_plugin_ip(IpAddr::V4(mapped));
            }
            !(address.is_loopback())
                && (address.is_unique_local()
                    || address.is_unicast_link_local()
                    || address.is_unspecified()
                    || address.is_multicast())
        }
    }
}

/// 插件/市场地址是服务端主动请求的目标。允许 loopback（本机插件），
/// 拒绝 RFC1918、链路本地、IPv6 ULA 和云元数据地址，避免 manifest/invoke 形成 SSRF。
async fn validate_plugin_endpoint(endpoint: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(endpoint.trim()).map_err(|e| format!("插件地址无效: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("插件地址仅支持 http(s)".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("插件地址不得包含用户名或密码".into());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "插件地址缺少主机名".to_string())?;
    // `Url::host_str()` 保留 IPv6 方括号，先剥离后再做 IpAddr 解析，
    // 否则 `[::ffff:10.0.0.8]` 等 IPv4-mapped 内网地址会绕过拦截。
    let host_for_parse = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    let lower = host_for_parse.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "metadata.google.internal" | "metadata" | "instance-data"
    ) || lower == "169.254.169.254"
    {
        return Err("拒绝访问云元数据地址".into());
    }
    if let Ok(ip) = host_for_parse.parse::<IpAddr>() {
        if blocked_plugin_ip(ip) {
            return Err("拒绝访问内网/链路本地插件地址".into());
        }
    } else {
        // DNS 解析后再次检查，降低 DNS rebinding 把公开域名解析到内网的风险。
        let port = url
            .port_or_known_default()
            .ok_or_else(|| "插件地址缺少有效端口".to_string())?;
        if let Ok(addresses) = tokio::net::lookup_host((host_for_parse, port)).await {
            for address in addresses {
                if blocked_plugin_ip(address.ip()) {
                    return Err("插件域名解析到内网/链路本地地址，已拒绝".into());
                }
            }
        }
    }
    Ok(url)
}

/// 校验 manifest(M3+M5 契约 v2):必填 id/name/version/mount/input/output;mount 须四挂载点之一。
/// v2 可选字段存在即验型:models 数组(第一项=主模型,按序故障转移)、config 数组(type 枚举
/// text|password|select|number|toggle|slider)、scenes 数组、md 字符串、ui 布尔。
pub fn validate_manifest(m: &Map<String, Value>) -> Result<(), String> {
    let req = ["id", "name", "version", "mount", "input", "output"];
    for k in req {
        if m.get(k).is_none() {
            return Err(format!("manifest 缺必填字段 {k}"));
        }
    }
    let id = m["id"].as_str().unwrap_or("");
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err("id 须为字母数字连字符".into());
    }
    let mount = m["mount"].as_str().unwrap_or("");
    if !MOUNTS.contains(&mount) {
        return Err(format!("mount 须为 {MOUNTS:?} 之一,得到 {mount:?}"));
    }
    for field in ["name", "version"] {
        if !m.get(field).is_some_and(Value::is_string) {
            return Err(format!("{field} 须为字符串"));
        }
    }
    for field in ["input", "output"] {
        if !m.get(field).is_some_and(Value::is_object) {
            return Err(format!("{field} 须为对象"));
        }
    }
    if let Some(icon) = m.get("icon") {
        let icon = icon
            .as_str()
            .ok_or_else(|| "icon 须为纯文本字符串".to_string())?;
        if icon.chars().count() > 32
            || icon
                .chars()
                .any(|c| c.is_control() || matches!(c, '<' | '>' | '&' | '"' | '\'' | '`'))
        {
            return Err("icon 只能是 32 字符以内的纯文本或 emoji".into());
        }
    }
    // ── v2 扩展字段(存在即验型)──
    if let Some(models) = m.get("models") {
        let arr = models
            .as_array()
            .ok_or("models 须为数组(第一项=主模型,按序故障转移)")?;
        for (i, mm) in arr.iter().enumerate() {
            let o = mm
                .as_object()
                .ok_or(format!("models[{i}] 须为对象 {{id, api, note}}"))?;
            if o.get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .is_empty()
            {
                return Err(format!("models[{i}] 缺 id(必填)"));
            }
        }
    }
    if let Some(config) = m.get("config") {
        let arr = config.as_array().ok_or("config 须为数组(配置项 schema)")?;
        for (i, c) in arr.iter().enumerate() {
            let o = c
                .as_object()
                .ok_or(format!("config[{i}] 须为对象 {{k, label, type, ...}}"))?;
            if o.get("k").and_then(|v| v.as_str()).unwrap_or("").is_empty() {
                return Err(format!("config[{i}] 缺 k(键名必填)"));
            }
            let ty = o.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if !["text", "password", "select", "number", "toggle", "slider"].contains(&ty) {
                return Err(format!(
                    "config[{i}] type 须为 text|password|select|number|toggle|slider 之一,得到 {ty:?}"
                ));
            }
        }
    }
    if let Some(scenes) = m.get("scenes") {
        scenes.as_array().ok_or("scenes 须为数组(应用场景列表)")?;
    }
    if let Some(md) = m.get("md") {
        if !md.is_string() {
            return Err("md 须为字符串(markdown 文档)".into());
        }
    }
    if let Some(ui) = m.get("ui") {
        if !ui.is_boolean() {
            return Err("ui 须为布尔(插件自带操作界面)".into());
        }
    }
    Ok(())
}

fn public_config_values(codex_home: &std::path::Path, entry: &registry::Entry) -> Value {
    let mut values = entry
        .config
        .get("values")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    for key in values.keys().cloned().collect::<Vec<_>>() {
        if registry::is_secret_config_key(entry, &key) {
            values.insert(
                key.clone(),
                json!(
                    if registry::get_plugin_secret(codex_home, &entry.id, &key).is_some() {
                        registry::SECRET_MASK
                    } else {
                        ""
                    }
                ),
            );
        }
    }
    Value::Object(values)
}

fn resolved_config_values(
    codex_home: &std::path::Path,
    entry: &registry::Entry,
) -> Map<String, Value> {
    let mut values = entry
        .config
        .get("values")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    for key in values.keys().cloned().collect::<Vec<_>>() {
        if registry::is_secret_config_key(entry, &key) {
            if let Some(secret) = registry::get_plugin_secret(codex_home, &entry.id, &key) {
                values.insert(key, json!(secret));
            } else {
                values.remove(&key);
            }
        }
    }
    values
}

/// 拉取并校验 manifest;Ok = manifest 全量(含 endpoint 回填)。
pub async fn fetch_manifest(endpoint: &str) -> Result<Map<String, Value>, String> {
    let validated = validate_plugin_endpoint(endpoint).await?;
    let base = validated.as_str().trim_end_matches('/').to_string();
    let resp = plugin_client()
        .get(format!("{base}/manifest"))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("插件不可达: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("manifest 拉取失败(HTTP {})", resp.status()));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| format!("manifest 非合法 JSON: {e}"))?;
    let mut m = v.as_object().cloned().ok_or("manifest 须为 JSON 对象")?;
    validate_manifest(&m)?;
    m.insert("endpoint".into(), json!(base));
    Ok(m)
}

/// 调用插件 /invoke,透传其 200 包错误;网络层失败人话映射。
pub async fn invoke_plugin(entry: &registry::Entry, body: &Value) -> Response {
    let Some(endpoint) = entry.meta.get("endpoint").and_then(|v| v.as_str()) else {
        return err_env(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_PLUGIN_NO_ENDPOINT",
            "插件条目缺 endpoint",
        );
    };
    let timeout_ms = entry
        .meta
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    let validated = match validate_plugin_endpoint(endpoint).await {
        Ok(value) => value,
        Err(error) => return err_env(StatusCode::BAD_REQUEST, "E_PLUGIN_ENDPOINT", &error),
    };
    let base = validated.as_str().trim_end_matches('/');
    match plugin_client()
        .post(format!("{base}/invoke"))
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .json(body)
        .send()
        .await
    {
        Ok(r) => {
            let status =
                StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            match r.json::<Value>().await {
                // 契约:成功/插件侧失败都是 200 + {ok:bool,...};网关原样透传
                Ok(v) => raw_json(status, &v),
                Err(e) => err_env(
                    StatusCode::BAD_GATEWAY,
                    "E_PLUGIN_BAD_RESP",
                    &format!("插件响应非 JSON: {e}"),
                ),
            }
        }
        Err(e) if e.is_timeout() => err_env(
            StatusCode::GATEWAY_TIMEOUT,
            "E_MEDIA_PLUGIN_TIMEOUT",
            &format!("插件响应超时(上限 {}ms),请检查插件服务", timeout_ms),
        ),
        Err(e) => err_env(
            StatusCode::BAD_GATEWAY,
            "E_MEDIA_PLUGIN_DOWN",
            &format!("插件不可达: {e}"),
        ),
    }
}

// ── HTTP handlers(server.rs build_router 注册)───────────────

use axum::extract::{Path, State};
use axum::routing::{get, post, put};
use axum::{Json, Router};

pub fn routes() -> Router<Arc<crate::server::AppState>> {
    Router::new()
        .route("/api/plugins", get(handle_list).post(handle_register))
        .route("/api/plugins/local", post(handle_local_add))
        .route("/api/plugins/:id", get(handle_detail).delete(handle_remove))
        .route("/api/plugins/:id/install", post(handle_install))
        .route("/api/plugins/:id/toggle", post(handle_toggle))
        .route("/api/plugins/:id/config", put(handle_config))
        .route("/api/plugins/:id/update", post(handle_update))
        .route("/api/plugins/:id/invoke", post(handle_invoke))
        .route("/api/plugin-market", get(handle_market_list))
        .route("/api/plugin-market/sources", post(handle_market_source_add))
        .route(
            "/api/plugin-market/sources/:id",
            axum::routing::delete(handle_market_source_remove),
        )
        .route(
            "/api/plugin-market/sources/:id/plugins",
            get(handle_source_plugins),
        )
        .route("/api/plugin-market/install", post(handle_market_install))
}

async fn handle_list(State(s): State<Arc<crate::server::AppState>>) -> Response {
    let entries = registry::list_json(&s.codex_home);
    // plugin=http 型,tool=官方内置能力(C 段条目靠此可见);model 条目(探测登记)不混入
    let plugins: Vec<Value> = entries["entries"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|e| e["kind"] == "plugin" || e["kind"] == "tool")
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    raw_json(StatusCode::OK, &json!({ "plugins": plugins }))
}

async fn handle_register(
    State(s): State<Arc<crate::server::AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let Some(endpoint) = body.get("endpoint").and_then(|v| v.as_str()).map(str::trim) else {
        return err_env(
            StatusCode::BAD_REQUEST,
            "E_ARGS",
            "endpoint 必填(http://host:port)",
        );
    };
    if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
        return err_env(
            StatusCode::BAD_REQUEST,
            "E_ARGS",
            "endpoint 须为 http(s):// 开头",
        );
    }
    let source_id = body.get("sourceId").and_then(|v| v.as_str()).unwrap_or("");
    match fetch_manifest(endpoint).await {
        Ok(mut m) => {
            if !source_id.is_empty() {
                m.insert("source_id".into(), json!(source_id));
            }
            let id = m
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            registry::upsert_plugin(&s.codex_home, &m);
            raw_json(
                StatusCode::OK,
                &json!({ "ok": true, "id": id, "manifest": m }),
            )
        }
        Err(e) => err_env(StatusCode::BAD_REQUEST, "E_PLUGIN_MANIFEST", &e),
    }
}

/// 本地添加:body=manifest JSON 对象(或 {file: manifest JSON 文本});校验通过 → 登记 registry(source=local,id 前缀 local.)。
async fn handle_local_add(
    State(s): State<Arc<crate::server::AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let parsed: Value = if let Some(f) = body.get("file").and_then(|v| v.as_str()) {
        match serde_json::from_str(f) {
            Ok(v) => v,
            Err(e) => {
                return err_env(
                    StatusCode::BAD_REQUEST,
                    "E_PLUGIN_MANIFEST",
                    &format!("file 内容非合法 JSON: {e}"),
                )
            }
        }
    } else {
        body.clone()
    };
    let Some(mut m) = parsed.as_object().cloned() else {
        return err_env(
            StatusCode::BAD_REQUEST,
            "E_ARGS",
            "body 须为 manifest JSON 对象(或 {file: manifest JSON 文本})",
        );
    };
    if let Err(e) = validate_manifest(&m) {
        return err_env(StatusCode::BAD_REQUEST, "E_PLUGIN_MANIFEST", &e);
    }
    let source = body
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("local");
    if !matches!(source, "local" | "paste") {
        return err_env(
            StatusCode::BAD_REQUEST,
            "E_ARGS",
            "source 须为 local|paste(默认 local)",
        );
    }
    m.insert("source".into(), json!(source));
    m.insert("source_id".into(), json!("local"));
    let pid = m
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    registry::upsert_plugin(&s.codex_home, &m);
    raw_json(
        StatusCode::OK,
        &json!({ "ok": true, "id": format!("local.{pid}"), "manifest": m }),
    )
}

/// 详情:manifest 全量 + 用户配置(models 优先级/failover/config_values)+ status/source/updated_at。
/// POST /api/plugins/:id/install —— 安装(官方/已登记条目 → registry 启用,version 以市场为准)。
async fn handle_install(
    State(s): State<Arc<crate::server::AppState>>,
    Path(id): Path<String>,
) -> Response {
    // 已登记 → 幂等启用;version 落后市场则同步(旧条目登记过旧版)
    if let Some(entry) = registry::get_plugin(&s.codex_home, &id) {
        let market = official_market();
        if let Some(m) = market["plugins"]
            .as_array()
            .and_then(|a| a.iter().find(|p| p["id"] == id))
        {
            let inst_v = entry
                .meta
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let mkt_v = m.get("version").and_then(|v| v.as_str()).unwrap_or("");
            if mkt_v > inst_v {
                let mut mm = m.as_object().cloned().unwrap_or_default();
                mm.insert("source".into(), json!("official"));
                mm.insert("builtin".into(), json!(true));
                registry::upsert_plugin(&s.codex_home, &mm);
            }
        }
        registry::set_enabled(&s.codex_home, &id, true);
        return raw_json(
            StatusCode::OK,
            &json!({ "ok": true, "installed": true, "id": id }),
        );
    }
    // 官方市场条目 → 取市场 manifest 登记(version 以市场为准,避免登记旧版致「可更新」误标)
    let market = official_market();
    if let Some(m) = market["plugins"]
        .as_array()
        .and_then(|a| a.iter().find(|p| p["id"] == id))
    {
        let mut mm = m.as_object().cloned().unwrap_or_default();
        mm.insert("source".into(), json!("official"));
        mm.insert("builtin".into(), json!(true));
        registry::upsert_plugin(&s.codex_home, &mm);
        registry::set_enabled(&s.codex_home, &id, true);
        return raw_json(
            StatusCode::OK,
            &json!({ "ok": true, "installed": true, "id": id }),
        );
    }
    err_env(
        StatusCode::NOT_FOUND,
        "E_NO_PLUGIN",
        "插件不存在(未上架或未登记,请先本地添加/注册)",
    )
}

async fn handle_detail(
    State(s): State<Arc<crate::server::AppState>>,
    Path(id): Path<String>,
) -> Response {
    let Some(entry) = registry::get_plugin(&s.codex_home, &id) else {
        return err_env(StatusCode::NOT_FOUND, "E_NO_PLUGIN", "插件不存在");
    };
    let mut d = entry.meta.clone();
    d.insert("enabled".into(), json!(entry.enabled));
    d.insert(
        "status".into(),
        json!(if entry.enabled { "enabled" } else { "disabled" }),
    );
    d.insert("source".into(), json!(entry.source));
    d.insert("updated_at".into(), json!(entry.updated_at));
    d.insert(
        "config_values".into(),
        public_config_values(&s.codex_home, &entry),
    );
    d.insert(
        "failover".into(),
        entry.config.get("failover").cloned().unwrap_or(json!(true)),
    );
    d.insert("models".into(), json!(user_models(&entry)));
    raw_json(StatusCode::OK, &json!({ "ok": true, "data": d }))
}

/// 保存配置:{config:{k:v}, models:[{id,api,note}], failover:bool} → 落 registry。
async fn handle_config(
    State(s): State<Arc<crate::server::AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let Some(entry) = registry::get_plugin(&s.codex_home, &id) else {
        return err_env(StatusCode::NOT_FOUND, "E_NO_PLUGIN", "插件不存在");
    };
    let Some(values) = body.get("config").and_then(|v| v.as_object()).cloned() else {
        return err_env(StatusCode::BAD_REQUEST, "E_ARGS", "config 须为对象 {k: v}");
    };
    // models 缺省 = 保留既有(部分保存);给出则须数组且每项 id 非空
    let models = match body.get("models") {
        Some(v) if v.is_array() => {
            let arr = v.as_array().unwrap();
            for (i, m) in arr.iter().enumerate() {
                if m.get("id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .is_empty()
                {
                    return err_env(
                        StatusCode::BAD_REQUEST,
                        "E_ARGS",
                        &format!("models[{i}] 缺 id"),
                    );
                }
            }
            Some(arr.clone())
        }
        Some(_) => {
            return err_env(
                StatusCode::BAD_REQUEST,
                "E_ARGS",
                "models 须为数组 [{id, api, note}, ...]",
            )
        }
        None => entry
            .config
            .get("models")
            .and_then(|v| v.as_array())
            .cloned(),
    };
    let failover = body
        .get("failover")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let mut config = Map::new();
    let mut public_values = Map::new();
    for (key, value) in &values {
        if registry::is_secret_config_key(&entry, key) {
            let secret = value.as_str().unwrap_or("");
            if secret != registry::SECRET_MASK {
                if let Err(error) =
                    registry::set_plugin_secret(&s.codex_home, &entry.id, key, secret)
                {
                    return err_env(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "E_PLUGIN_SECRET_SAVE",
                        &error,
                    );
                }
            }
            public_values.insert(
                key.clone(),
                json!(
                    if registry::get_plugin_secret(&s.codex_home, &entry.id, key).is_some() {
                        registry::SECRET_MASK
                    } else {
                        ""
                    }
                ),
            );
        } else {
            public_values.insert(key.clone(), value.clone());
        }
    }
    config.insert("models".into(), json!(models.unwrap_or_default()));
    config.insert("failover".into(), json!(failover));
    config.insert("values".into(), Value::Object(public_values));
    if !registry::set_config(&s.codex_home, &id, config.clone()) {
        return err_env(StatusCode::NOT_FOUND, "E_NO_PLUGIN", "插件不存在");
    }
    raw_json(
        StatusCode::OK,
        &json!({ "ok": true, "id": id, "config": config, "config_values": public_config_values(&s.codex_home, &entry) }),
    )
}

/// 更新:官方/远程条目重新拉 manifest 比对版本;本地条目 400 人话「本地插件更新请重新添加」。
async fn handle_update(
    State(s): State<Arc<crate::server::AppState>>,
    Path(id): Path<String>,
) -> Response {
    let Some(entry) = registry::get_plugin(&s.codex_home, &id) else {
        return err_env(StatusCode::NOT_FOUND, "E_NO_PLUGIN", "插件不存在");
    };
    if entry.source == "local" || entry.id.starts_with("local.") {
        return err_env(
            StatusCode::BAD_REQUEST,
            "E_LOCAL_UPDATE",
            "本地插件更新请重新添加",
        );
    }
    let base_id = entry
        .meta
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or(&entry.id);
    let new_manifest: Map<String, Value> = if entry
        .meta
        .get("builtin")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        match official_market()["plugins"]
            .as_array()
            .and_then(|a| a.iter().find(|p| p["id"] == base_id).cloned())
            .and_then(|p| p.as_object().cloned())
        {
            Some(m) => m,
            None => return err_env(StatusCode::NOT_FOUND, "E_NO_PLUGIN", "官方源无该插件"),
        }
    } else {
        // 远程:源清单 → 条目 endpoint → 重新拉 manifest
        let sid = entry
            .meta
            .get("source_id")
            .and_then(|v| v.as_str())
            .unwrap_or(&entry.source);
        let Some(url) = load_sources(&s.codex_home)
            .into_iter()
            .find(|x| x["id"] == sid)
            .and_then(|x| x["url"].as_str().map(String::from))
        else {
            return err_env(StatusCode::NOT_FOUND, "E_NO_SOURCE", "源不存在,请先添加");
        };
        let market = match fetch_market(&url).await {
            Ok(v) => v,
            Err(e) => return err_env(StatusCode::BAD_GATEWAY, "E_SOURCE_FETCH", &e),
        };
        let Some(item) = market["plugins"]
            .as_array()
            .and_then(|a| a.iter().find(|p| p["id"] == base_id).cloned())
        else {
            return err_env(StatusCode::NOT_FOUND, "E_NO_PLUGIN", "该源无此插件");
        };
        let Some(endpoint) = item.get("endpoint").and_then(|v| v.as_str()) else {
            return err_env(
                StatusCode::BAD_REQUEST,
                "E_PLUGIN_MANIFEST",
                "清单条目缺 endpoint",
            );
        };
        let mut m = match fetch_manifest(endpoint).await {
            Ok(m) => m,
            Err(e) => return err_env(StatusCode::BAD_REQUEST, "E_PLUGIN_MANIFEST", &e),
        };
        m.insert("source_id".into(), json!(sid));
        m
    };
    let new_ver = new_manifest
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let old_ver = entry
        .meta
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if new_ver == old_ver {
        return raw_json(
            StatusCode::OK,
            &json!({ "ok": true, "updated": false, "version": new_ver, "id": id }),
        );
    }
    registry::upsert_plugin(&s.codex_home, &new_manifest); // 保留用户 config(upsert 不动 config)
    raw_json(
        StatusCode::OK,
        &json!({ "ok": true, "updated": true, "version": new_ver, "id": id }),
    )
}

async fn handle_remove(
    State(s): State<Arc<crate::server::AppState>>,
    Path(id): Path<String>,
) -> Response {
    registry::remove(&s.codex_home, &id);
    raw_json(StatusCode::OK, &json!({ "ok": true }))
}

async fn handle_toggle(
    State(s): State<Arc<crate::server::AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let enabled = body
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    registry::set_enabled(&s.codex_home, &id, enabled);
    raw_json(
        StatusCode::OK,
        &json!({ "ok": true, "id": id, "enabled": enabled }),
    )
}

async fn handle_invoke(
    State(s): State<Arc<crate::server::AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let Some(entry) = registry::get_plugin(&s.codex_home, &id) else {
        return err_env(StatusCode::NOT_FOUND, "E_NO_PLUGIN", "插件不存在");
    };
    if !entry.enabled {
        return err_env(
            StatusCode::FORBIDDEN,
            "E_PLUGIN_DISABLED",
            "插件已停用,请先在插件管理启用",
        );
    }
    // 官方内置 tool:按条目 id 分发本机实现(注册表统一管理,走同一 invoke 入口,非特权通道)
    if entry.kind == registry::Kind::Tool
        && entry
            .meta
            .get("builtin")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    {
        match entry.id.as_str() {
            "ffmpeg-frame-extract" => {
                let values = resolved_config_values(&s.codex_home, &entry);
                return builtin_frame_extract_config(&s.codex_home, &body, Some(&values)).await;
            }
            "image-describe" => {
                return builtin_media_failover(&s, &entry, &body, MediaTool::Describe).await
            }
            "image-generate" => {
                return builtin_media_failover(&s, &entry, &body, MediaTool::Generate).await
            }
            "image-edit" => {
                return builtin_media_failover(&s, &entry, &body, MediaTool::Edit).await
            }
            "asr-speech" => {
                let values = resolved_config_values(&s.codex_home, &entry);
                return builtin_asr_speech(&s.codex_home, &entry, &body, &values).await;
            }
            "tts-speech" => {
                let values = resolved_config_values(&s.codex_home, &entry);
                return builtin_tts_speech(&s.codex_home, &entry, &body, &values).await;
            }
            _ => {}
        }
    }
    let values = resolved_config_values(&s.codex_home, &entry);
    invoke_with_failover_config(&entry, &body, Some(&values)).await
}

/// 内置媒体工具分发(media_tools 三函数签名同构,枚举免去泛型 future 生命周期体操)。
enum MediaTool {
    Describe,
    Generate,
    Edit,
}

/// 内置媒体工具(识图/文生图/图编辑)故障转移,复用 http 型链模式:
/// 条目配置了 models 且 failover 开启 → 按 models 优先级逐个尝试(主败切备用,全败聚合人话),
/// 每个模型注入 model/api 覆盖进请求体(media_tools 的 api_base 据此换端点);
/// 未配置(models 空)→ 原行为(active 供应商直连);单模型/关 failover → 只试主模型。
async fn builtin_media_failover(
    s: &AppState,
    entry: &registry::Entry,
    body: &Value,
    tool: MediaTool,
) -> Response {
    let models = user_models(entry);
    if models.is_empty() {
        return match tool {
            MediaTool::Describe => crate::media_tools::image_describe(s, body).await,
            MediaTool::Generate => crate::media_tools::image_generate(s, body).await,
            MediaTool::Edit => crate::media_tools::image_edit(s, body).await,
        };
    }
    let failover = entry
        .config
        .get("failover")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let pool: Vec<&Value> = if models.len() == 1 || !failover {
        vec![&models[0]]
    } else {
        models.iter().collect()
    };
    let mut parts: Vec<String> = Vec::new();
    for (i, m) in pool.iter().enumerate() {
        let label = if i == 0 { "主模型" } else { "备用模型" };
        let mid = m.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let mut req = body.clone();
        req["model"] = json!(mid);
        let resp = match tool {
            MediaTool::Describe => crate::media_tools::image_describe(s, &req).await,
            MediaTool::Generate => crate::media_tools::image_generate(s, &req).await,
            MediaTool::Edit => crate::media_tools::image_edit(s, &req).await,
        };
        let bytes = match axum::body::to_bytes(resp.into_body(), usize::MAX).await {
            Ok(b) => b,
            Err(_) => {
                parts.push(format!("{label} {mid} 失败(响应读取失败)"));
                continue;
            }
        };
        let v: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => {
                parts.push(format!("{label} {mid} 失败(响应非 JSON)"));
                continue;
            }
        };
        // 媒体工具契约:成功/业务失败均包 200(ok:true 才放行,失败切备用)
        if v.get("ok").and_then(|x| x.as_bool()) == Some(true) {
            return raw_json(StatusCode::OK, &v);
        }
        let reason = v["error"]["human"]
            .as_str()
            .or_else(|| v["error"]["message"].as_str())
            .unwrap_or("业务错误")
            .to_string();
        parts.push(format!("{label} {mid} 失败({reason})"));
    }
    err_env(
        StatusCode::BAD_GATEWAY,
        "E_PLUGIN_FAILOVER",
        &format!("{}。请检查配置或稍后重试", parts.join(",")),
    )
}

/// models 优先级:用户配置(config.models)优先,退 manifest 声明(meta.models)。
fn user_models(entry: &registry::Entry) -> Vec<Value> {
    if let Some(arr) = entry.config.get("models").and_then(|v| v.as_array()) {
        if !arr.is_empty() {
            return arr.clone();
        }
    }
    entry
        .meta
        .get("models")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

#[derive(Clone)]
struct SpeechEndpoint {
    api: String,
    model: String,
}

const MAX_ASR_INPUT_BYTES: u64 = 25 * 1024 * 1024;
const MAX_ASR_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_TTS_RESPONSE_BYTES: usize = 25 * 1024 * 1024;
const MAX_VIDEO_INPUT_BYTES: u64 = 100 * 1024 * 1024;

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn validate_keyed_endpoint(api_base: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(api_base).map_err(|error| format!("API 地址无效: {error}"))?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if parsed.host_str().is_some_and(is_loopback_host) => Ok(()),
        "http" => Err("拒绝向非回环明文 HTTP 地址发送 API Key,请改用 HTTPS".into()),
        _ => Err("API 地址仅支持 HTTPS，或本机回环 HTTP".into()),
    }
}

async fn response_bytes_limited(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(format!("响应超过大小上限 {max_bytes} bytes"));
    }
    let mut data = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("响应读取失败: {error}"))?
    {
        if data.len().saturating_add(chunk.len()) > max_bytes {
            return Err(format!("响应超过大小上限 {max_bytes} bytes"));
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

fn config_text<'a>(config: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    config
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn body_text<'a>(body: &'a Value, key: &str) -> Option<&'a str> {
    body.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn speech_endpoints(
    entry: &registry::Entry,
    body: &Value,
    config: &Map<String, Value>,
    default_model: &str,
) -> Vec<SpeechEndpoint> {
    let explicit_model = body_text(body, "model");
    let configured_model = config_text(config, "model");
    let configured_api = config_text(config, "apiBase");
    let models = user_models(entry);
    let mut endpoints = Vec::new();

    for model in &models {
        let model_api = model
            .get("api")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let Some(api) = model_api.or(configured_api) else {
            continue;
        };
        let manifest_model = model
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let selected_model = if model_api.is_some() {
            explicit_model.or(manifest_model).or(configured_model)
        } else {
            explicit_model.or(configured_model).or(manifest_model)
        }
        .unwrap_or(default_model);
        if endpoints.iter().any(|endpoint: &SpeechEndpoint| {
            endpoint.api == api && endpoint.model == selected_model
        }) {
            continue;
        }
        endpoints.push(SpeechEndpoint {
            api: api.to_string(),
            model: selected_model.to_string(),
        });
    }

    if let Some(api) = configured_api {
        if !endpoints.iter().any(|endpoint| endpoint.api == api) {
            let fallback_model = models
                .first()
                .and_then(|model| model.get("id"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty());
            endpoints.push(SpeechEndpoint {
                api: api.to_string(),
                model: explicit_model
                    .or(configured_model)
                    .or(fallback_model)
                    .unwrap_or(default_model)
                    .to_string(),
            });
        }
    }

    let failover = entry
        .config
        .get("failover")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if !failover && endpoints.len() > 1 {
        endpoints.truncate(1);
    }
    endpoints
}

fn openai_audio_url(api_base: &str, operation: &str) -> String {
    let base = api_base.trim_end_matches('/');
    let endpoint = format!("/audio/{operation}");
    if base.ends_with(&endpoint) {
        base.to_string()
    } else if base.ends_with("/v1") {
        format!("{base}{endpoint}")
    } else {
        format!("{base}/v1{endpoint}")
    }
}

fn resolve_media_path(
    codex_home: &std::path::Path,
    media_url: &str,
    max_bytes: u64,
) -> Result<std::path::PathBuf, (&'static str, String)> {
    let media_url = media_url.trim();
    if media_url.contains("..") {
        return Err(("E_ARGS", "media_url 不得包含路径穿越片段".into()));
    }
    let media_root = crate::media::media_root(codex_home);
    let candidate = if let Some(tail) = media_url.rsplit('/').next() {
        if let Some((id, ext)) = tail.split_once('.') {
            if !id.is_empty()
                && id.chars().all(|c| c.is_ascii_hexdigit())
                && !ext.is_empty()
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
            {
                media_root.join(tail)
            } else {
                std::path::PathBuf::from(media_url)
            }
        } else {
            std::path::PathBuf::from(media_url)
        }
    } else {
        std::path::PathBuf::from(media_url)
    };
    if !candidate.is_absolute() {
        return Err(("E_ARGS", "media_url 形态不支持".into()));
    }
    let root = std::fs::canonicalize(&media_root)
        .map_err(|_| ("E_MEDIA_NOT_FOUND", "媒体暂存目录不存在".into()))?;
    let path = std::fs::canonicalize(&candidate)
        .map_err(|_| ("E_MEDIA_NOT_FOUND", "媒体文件不存在".into()))?;
    if !path.starts_with(&root) {
        return Err(("E_MEDIA_SCOPE", "仅允许读取应用媒体暂存目录中的文件".into()));
    }
    let metadata = std::fs::metadata(&path)
        .map_err(|error| ("E_MEDIA_READ", format!("读取媒体元数据失败: {error}")))?;
    if !metadata.is_file() {
        return Err(("E_MEDIA_READ", "媒体地址必须指向普通文件".into()));
    }
    if metadata.len() > max_bytes {
        return Err((
            "E_MEDIA_TOO_LARGE",
            format!("媒体文件超过 {}MB 上限", max_bytes / 1024 / 1024),
        ));
    }
    Ok(path)
}

fn audio_mime_for_path(path: &std::path::Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "mp3" => Some("audio/mpeg"),
        "wav" => Some("audio/wav"),
        "m4a" => Some("audio/mp4"),
        "mp4" => Some("audio/mp4"),
        "webm" => Some("audio/webm"),
        "ogg" | "oga" => Some("audio/ogg"),
        "flac" => Some("audio/flac"),
        _ => None,
    }
}

fn speech_config_error(message: &str) -> Response {
    err_env(StatusCode::BAD_REQUEST, "E_PLUGIN_CONFIG", message)
}

fn speech_failover_error(parts: &[String]) -> Response {
    err_env(
        StatusCode::BAD_GATEWAY,
        "E_PLUGIN_FAILOVER",
        &format!("{}。请检查配置或稍后重试", parts.join(",")),
    )
}

async fn attempt_asr(
    endpoint: &SpeechEndpoint,
    api_key: &str,
    media_path: &std::path::Path,
    timeout_ms: u64,
) -> Result<String, String> {
    validate_keyed_endpoint(&endpoint.api)?;
    let metadata = tokio::fs::metadata(media_path)
        .await
        .map_err(|error| format!("读取音频元数据失败: {error}"))?;
    if metadata.len() > MAX_ASR_INPUT_BYTES {
        return Err(format!(
            "音频文件超过 {}MB 上限",
            MAX_ASR_INPUT_BYTES / 1024 / 1024
        ));
    }
    let data = tokio::fs::read(media_path)
        .await
        .map_err(|error| format!("读取音频失败: {error}"))?;
    let filename = media_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("audio.bin")
        .to_string();
    let mut file = reqwest::multipart::Part::bytes(data).file_name(filename);
    if let Some(mime) = audio_mime_for_path(media_path) {
        file = file
            .mime_str(mime)
            .map_err(|error| format!("音频 MIME 无效: {error}"))?;
    }
    let form = reqwest::multipart::Form::new()
        .part("file", file)
        .text("model", endpoint.model.clone());
    let response = plugin_client()
        .post(openai_audio_url(&endpoint.api, "transcriptions"))
        .bearer_auth(api_key)
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .multipart(form)
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                format!("响应超时(上限 {timeout_ms}ms)")
            } else {
                format!("不可达: {error}")
            }
        })?;
    let status = response.status();
    let bytes = response_bytes_limited(response, MAX_ASR_RESPONSE_BYTES).await?;
    if !status.is_success() {
        return Err(format!(
            "上游错误(HTTP {}): {}",
            status.as_u16(),
            String::from_utf8_lossy(&bytes)
                .chars()
                .take(200)
                .collect::<String>()
        ));
    }
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|error| format!("响应非 JSON: {error}"))?;
    value
        .get("text")
        .and_then(Value::as_str)
        .or_else(|| value.get("data")?.get("text")?.as_str())
        .map(str::to_string)
        .ok_or_else(|| "响应缺少 text".to_string())
}

async fn builtin_asr_speech(
    codex_home: &std::path::Path,
    entry: &registry::Entry,
    body: &Value,
    config: &Map<String, Value>,
) -> Response {
    let Some(media_url) = body_text(body, "media_url") else {
        return raw_json(
            StatusCode::OK,
            &json!({"ok": false, "error": {"code": "E_ARGS", "message": "media_url 必填", "human": "请提供待识别音频的媒体地址"}}),
        );
    };
    let media_path = match resolve_media_path(codex_home, media_url, MAX_ASR_INPUT_BYTES) {
        Ok(path) => path,
        Err((code, message)) => {
            return raw_json(
                StatusCode::OK,
                &json!({"ok": false, "error": {"code": code, "message": message, "human": "请先上传音频并使用网关暂存地址"}}),
            );
        }
    };
    let Some(api_key) = config_text(config, "apiKey") else {
        return speech_config_error("请先配置 ASR API Key");
    };
    let endpoints = speech_endpoints(entry, body, config, "whisper-1");
    if endpoints.is_empty() {
        return speech_config_error("请先配置 ASR apiBase 或模型端点");
    }
    let timeout_ms = entry
        .meta
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    let mut parts = Vec::new();
    for (index, endpoint) in endpoints.iter().enumerate() {
        match attempt_asr(endpoint, api_key, &media_path, timeout_ms).await {
            Ok(text) => {
                return raw_json(StatusCode::OK, &json!({"ok": true, "data": {"text": text}}));
            }
            Err(reason) => parts.push(format!(
                "{} {} 失败({reason})",
                if index == 0 {
                    "主端点"
                } else {
                    "备用端点"
                },
                endpoint.model
            )),
        }
    }
    speech_failover_error(&parts)
}

fn response_audio_mime(content_type: Option<&str>) -> &'static str {
    let value = content_type.unwrap_or("").to_ascii_lowercase();
    if value.starts_with("audio/mpeg") || value.starts_with("audio/mp3") {
        "audio/mpeg"
    } else if value.starts_with("audio/wav")
        || value.starts_with("audio/x-wav")
        || value.starts_with("audio/wave")
    {
        "audio/wav"
    } else {
        ""
    }
}

async fn attempt_tts(
    codex_home: &std::path::Path,
    endpoint: &SpeechEndpoint,
    api_key: &str,
    text: &str,
    voice: &str,
    timeout_ms: u64,
) -> Result<Value, String> {
    validate_keyed_endpoint(&endpoint.api)?;
    let response = plugin_client()
        .post(openai_audio_url(&endpoint.api, "speech"))
        .bearer_auth(api_key)
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .json(&json!({
            "model": endpoint.model,
            "input": text,
            "voice": voice,
        }))
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                format!("响应超时(上限 {timeout_ms}ms)")
            } else {
                format!("不可达: {error}")
            }
        })?;
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let bytes = response_bytes_limited(response, MAX_TTS_RESPONSE_BYTES).await?;
    if !status.is_success() {
        return Err(format!(
            "上游错误(HTTP {}): {}",
            status.as_u16(),
            String::from_utf8_lossy(&bytes)
                .chars()
                .take(200)
                .collect::<String>()
        ));
    }
    if bytes.is_empty() {
        return Err("上游返回空音频".into());
    }
    let mime = response_audio_mime(content_type.as_deref());
    let item = crate::media::store_upload(codex_home, &bytes, mime, "tts-speech")
        .map_err(|(_, message)| format!("音频暂存失败: {message}"))?;
    Ok(json!({
        "media_url": format!("/media/{}.{}", item.id, item.ext),
        "mime": item.mime,
    }))
}

async fn builtin_tts_speech(
    codex_home: &std::path::Path,
    entry: &registry::Entry,
    body: &Value,
    config: &Map<String, Value>,
) -> Response {
    let Some(text) = body_text(body, "text") else {
        return raw_json(
            StatusCode::OK,
            &json!({"ok": false, "error": {"code": "E_ARGS", "message": "text 必填", "human": "请输入要合成的文字"}}),
        );
    };
    let Some(api_key) = config_text(config, "apiKey") else {
        return speech_config_error("请先配置 TTS API Key");
    };
    let voice = body_text(body, "voice")
        .or_else(|| config_text(config, "voice"))
        .unwrap_or("alloy");
    let endpoints = speech_endpoints(entry, body, config, "tts-1");
    if endpoints.is_empty() {
        return speech_config_error("请先配置 TTS apiBase 或模型端点");
    }
    let timeout_ms = entry
        .meta
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    let mut parts = Vec::new();
    for (index, endpoint) in endpoints.iter().enumerate() {
        match attempt_tts(codex_home, endpoint, api_key, text, voice, timeout_ms).await {
            Ok(data) => {
                return raw_json(StatusCode::OK, &json!({"ok": true, "data": data}));
            }
            Err(reason) => parts.push(format!(
                "{} {} 失败({reason})",
                if index == 0 {
                    "主端点"
                } else {
                    "备用端点"
                },
                endpoint.model
            )),
        }
    }
    speech_failover_error(&parts)
}

/// 单模型调用:POST {base}/invoke(model 注入 body);Ok=HTTP <500 响应体(含 200 包业务错误);
/// Err=网络错/5xx/超时/非 JSON(人话原因)。
async fn attempt_invoke(
    base: &str,
    model: &str,
    body: &Value,
    timeout_ms: u64,
) -> Result<(u16, Value), String> {
    let base = base.trim_end_matches('/');
    let mut req_body = body.clone();
    req_body["model"] = json!(model);
    match plugin_client()
        .post(format!("{base}/invoke"))
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .json(&req_body)
        .send()
        .await
    {
        Ok(r) => {
            let status = r.status().as_u16();
            if status >= 500 {
                return Err(format!("上游错误(HTTP {status})"));
            }
            match r.json::<Value>().await {
                Ok(v) => Ok((status, v)),
                Err(e) => Err(format!("响应非 JSON: {e}")),
            }
        }
        Err(e) if e.is_timeout() => Err(format!("响应超时(上限 {timeout_ms}ms)")),
        Err(e) => Err(format!("不可达: {e}")),
    }
}

/// 故障转移调用链:无 models → 原 endpoint 调用(行为不变);单模型/关闭 failover → 只试主模型
/// (结果原样返回);多模型 → 按优先级逐个尝试(网络错/5xx/超时/业务错都切换),全败聚合人话。
#[cfg(test)]
async fn invoke_with_failover(entry: &registry::Entry, body: &Value) -> Response {
    invoke_with_failover_config(entry, body, None).await
}

async fn invoke_with_failover_config(
    entry: &registry::Entry,
    body: &Value,
    config: Option<&Map<String, Value>>,
) -> Response {
    let mut payload = body.clone();
    if let Some(config) = config {
        payload["config"] = Value::Object(config.clone());
    }
    let timeout_ms = entry
        .meta
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    let models = user_models(entry);
    if models.is_empty() {
        return invoke_plugin(entry, &payload).await;
    }
    let failover = entry
        .config
        .get("failover")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let pool: Vec<&Value> = if models.len() == 1 || !failover {
        vec![&models[0]]
    } else {
        models.iter().collect()
    };
    let model_label = |i: usize| if i == 0 { "主模型" } else { "备用模型" };
    if pool.len() == 1 {
        let m = pool[0];
        let mid = m.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let api = m.get("api").and_then(|v| v.as_str()).unwrap_or("");
        if api.is_empty() {
            return err_env(
                StatusCode::BAD_GATEWAY,
                "E_PLUGIN_FAILOVER",
                &format!("主模型 {mid} 未配置服务端点(api 为空),请检查插件配置"),
            );
        }
        return match attempt_invoke(api, mid, &payload, timeout_ms).await {
            Ok((status, v)) => raw_json(StatusCode::from_u16(status).unwrap_or(StatusCode::OK), &v),
            Err(reason) => err_env(
                StatusCode::BAD_GATEWAY,
                "E_PLUGIN_FAILOVER",
                &format!("主模型 {mid} 失败({reason}),请检查配置或稍后重试"),
            ),
        };
    }
    // 多模型故障转移链
    let mut parts: Vec<String> = Vec::new();
    for (i, m) in pool.iter().enumerate() {
        let label = model_label(i);
        let mid = m.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let api = m.get("api").and_then(|v| v.as_str()).unwrap_or("");
        if api.is_empty() {
            parts.push(format!("{label} {mid} 未配置服务端点(api 为空)"));
            continue;
        }
        match attempt_invoke(api, mid, &payload, timeout_ms).await {
            Ok((status, v)) if v.get("ok").and_then(|x| x.as_bool()) == Some(true) => {
                return raw_json(StatusCode::from_u16(status).unwrap_or(StatusCode::OK), &v);
            }
            Ok((_, v)) => {
                let reason = v["error"]["human"]
                    .as_str()
                    .or_else(|| v["error"]["message"].as_str())
                    .unwrap_or("业务错误")
                    .to_string();
                parts.push(format!("{label} {mid} 失败({reason})"));
            }
            Err(reason) => parts.push(format!("{label} {mid} 失败({reason})")),
        }
    }
    err_env(
        StatusCode::BAD_GATEWAY,
        "E_PLUGIN_FAILOVER",
        &format!("{}。请检查配置或稍后重试", parts.join(",")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// mock 插件:/manifest(M3 形态)+ /invoke(成功 op 与未知 op 两形态)。
    async fn spawn_plugin() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    let Ok(n) = sock.read(&mut buf).await else {
                        return;
                    };
                    let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let (path, body) = match req.find("\r\n\r\n") {
                        Some(i) => {
                            let first = &req[..i];
                            let p = first.split_whitespace().nth(1).unwrap_or("");
                            (p.to_string(), req[i + 4..].to_string())
                        }
                        None => (String::new(), String::new()),
                    };
                    let resp_body = if path == "/manifest" {
                        r#"{"id":"ffmpeg-frame-extract","name":"抽帧","version":"1.0.0","mount":"media_parse","input":{"required":["media_url"],"properties":{}},"output":{"properties":{}},"timeout_ms":2000}"#.to_string()
                    } else if body.contains("\"known\"") {
                        r#"{"ok":true,"data":{"image_b64":"x"}}"#.to_string()
                    } else {
                        r#"{"ok":false,"error":{"code":"E_OP","message":"unknown","human":"未知操作"}}"#.to_string()
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        resp_body.len(),
                        resp_body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    fn entry_with(endpoint: &str, timeout_ms: u64) -> registry::Entry {
        let mut meta = Map::new();
        meta.insert("endpoint".into(), json!(endpoint));
        meta.insert("timeout_ms".into(), json!(timeout_ms));
        registry::Entry {
            id: "t".into(),
            kind: registry::Kind::Plugin,
            provider_id: None,
            model: None,
            enabled: true,
            meta,
            config: Map::new(),
            source: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn manifest_validation() {
        let good: Map<String, Value> = serde_json::from_value(json!({
            "id":"a-b","name":"n","version":"1.0.0","mount":"media_parse","input":{},"output":{}
        }))
        .unwrap();
        assert!(validate_manifest(&good).is_ok());
        assert!(validate_manifest(&{
            let mut m = good.clone();
            m.remove("mount");
            m
        })
        .is_err());
        assert!(validate_manifest(&{
            let mut m = good.clone();
            m.insert("mount".into(), json!("nope"));
            m
        })
        .is_err());
        assert!(validate_manifest(&{
            let mut m = good.clone();
            m.insert("icon".into(), json!("<img src=x onerror=alert(1)>"));
            m
        })
        .is_err());
    }

    #[tokio::test]
    async fn plugin_endpoint_rejects_private_and_credential_urls() {
        assert!(validate_plugin_endpoint("http://10.0.0.8:8080")
            .await
            .is_err());
        assert!(validate_plugin_endpoint("http://169.254.169.254/latest")
            .await
            .is_err());
        assert!(validate_plugin_endpoint("http://[::ffff:10.0.0.8]:8080")
            .await
            .is_err());
        assert!(
            validate_plugin_endpoint("https://user:pass@example.com/plugin")
                .await
                .is_err()
        );
        assert!(validate_plugin_endpoint("http://127.0.0.1:8787/plugin")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn register_validate_and_invoke_roundtrip() {
        let base = spawn_plugin().await;
        // 登记:manifest 拉取+校验+endpoint 回填
        let m = fetch_manifest(&base).await.unwrap();
        assert_eq!(m["id"], "ffmpeg-frame-extract");
        assert_eq!(m["endpoint"], base);
        // 调用:成功 op 透传
        let e = entry_with(&base, 2000);
        let resp = invoke_plugin(&e, &json!({"op":"known"})).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["ok"], true);
        // 插件侧失败也 200 包错误,透传
        let resp = invoke_plugin(&e, &json!({"op":"zzz"})).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["human"], "未知操作");
    }

    #[tokio::test]
    async fn invoke_timeout_maps_human_error() {
        let e = entry_with("http://127.0.0.1:9", 300); // 不可达端口
        let resp = invoke_plugin(&e, &json!({"op":"x"})).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let v: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v["error"]["code"], "E_MEDIA_PLUGIN_DOWN");
    }
}

// ── 插件市场(M5 定案:静态 JSON 清单;官方源内置,第三方仅 http(s) 清单拉取)──

pub const OFFICIAL_SOURCE: &str = "official";

/// 官方源内置清单:官方 6 条(识图/文生图/图编辑/抽帧/ASR/TTS),统一登记为内置 tool。
/// ASR/TTS 在本机完成 OpenAI 兼容协议适配,端点默认留空并由用户配置。
/// 数据与 `插件演示-产品UI/` PLUG2_DATA 一致(含 md/scenes/config/models 全字段)。
fn official_market() -> Value {
    json!({
        "schema_version": 1,
        "source": { "id": OFFICIAL_SOURCE, "name": "官方源" },
        "plugins": [
            {
                "id": "ffmpeg-frame-extract",
                "name": "视频抽帧 · 视频转图片",
                "author": "官方",
                "version": "1.0.1",
                "cap": "抽帧",
                "icon": "🎞️",
                "mount": "media_parse",
                "builtin": true,
                "ui": true,
                "short_desc": "视频抽帧",
                "desc": "按时间点从视频抽出一帧 JPEG,喂给识图类模型(路由插件示例:本机 ffmpeg 实现)。",
                "input": { "required": ["media_url"], "properties": { "media_url": "string", "t": "number" } },
                "output": { "properties": { "media_url": "string", "mime": "string" } },
                "timeout_ms": 60000,
                "models": [],
                "config": [
                    { "k": "ffmpegPath", "label": "ffmpeg 路径", "type": "text", "def": "ffmpeg", "hint": "留空用 PATH 中的 ffmpeg" },
                    { "k": "defaultT", "label": "默认时间点(秒)", "type": "number", "def": 0 }
                ],
                "scenes": [
                    { "title": "让模型「看懂视频」", "desc": "视频不能直接喂给模型——先按时间点抽帧成图,再经识图插件转成文字描述,模型即可回答视频内容问题(视频问答、内容摘要、监控画面分析)。" },
                    { "title": "素材提取", "desc": "从视频取关键帧做封面、缩略图、归档样本。" }
                ],
                "md": "# 视频抽帧 · 视频转图片\n\n> ai-gateway 路由插件 · 官方 · 挂载点 media_parse\n\n## 功能简介\n\n按时间点从视频抽出一帧 JPEG,喂给识图类模型(路由插件示例:本机 ffmpeg 实现)。\n\n## 应用场景\n\n### 让模型「看懂视频」\n\n视频不能直接喂给模型——先按时间点抽帧成图,再经识图插件转成文字描述,模型即可回答视频内容问题(视频问答、内容摘要、监控画面分析)。\n\n### 素材提取\n\n从视频取关键帧做封面、缩略图、归档样本。\n\n## 调用的模型能力\n\n调用本机 ffmpeg(-ss 定点抽帧),产物 JPEG 入媒体暂存,可喂识图管线。\n\n## 使用说明\n\n1. 本机需安装 ffmpeg(缺失时调用返回人话 E_MEDIA_FFMPEG)\n2. 输入限网关暂存(hex 校验防路径穿越)或本机绝对路径\n3. 抽帧产物可立即喂给识图插件做画面理解"
            },
            {
                "id": "image-describe",
                "name": "识图 · 图片转文字",
                "author": "官方",
                "version": "2.0.1",
                "cap": "识图",
                "icon": "🖼️",
                "mount": "media_parse",
                "builtin": true,
                "ui": true,
                "short_desc": "图片转文字描述",
                "desc": "上传/引用一张图片,调用上游 chat 模型带图识别,输出文字描述。",
                "input": { "required": ["media_url"], "properties": { "media_url": "string", "prompt": "string", "provider_id": "string", "model": "string" } },
                "output": { "properties": { "text": "string", "provider_id": "string", "model": "string" } },
                "timeout_ms": 120000,
                "models": [
                    { "id": "gpt-5.6", "api": "https://2xa.cc.cd/v1", "note": "主模型:多模态直接识别" },
                    { "id": "claude-fable-5", "api": "https://2xa.cc.cd/v1", "note": "备用:多模态直连" }
                ],
                "config": [
                    { "k": "model", "label": "识别模型", "type": "select", "def": "跟随供应商默认", "options": ["跟随供应商默认", "gpt-5.6", "claude-fable-5"], "hint": "能力探测 image_in=Yes 的模型" },
                    { "k": "lang", "label": "输出语言", "type": "select", "def": "中文", "options": ["中文", "English"] }
                ],
                "scenes": [
                    { "title": "给非多模态模型装上「眼睛」", "desc": "DeepSeek、GLM 等纯文本模型不能直接接收图片。本插件先把图片转成文字描述,再喂给模型——模型即可回答图片相关问题(截图理解、图表解读、商品图描述、验证码识别),原本非多模态的模型经此链路即获得识图能力。" },
                    { "title": "多模态链路中转站", "desc": "视频抽帧产物、设计稿、文档截图等,先经识图转成文字,再进入对话/总结/文档流程,全链路统一走文字通道,任何模型都能参与。" },
                    { "title": "批量图片描述", "desc": "媒体暂存里的一批图片,逐一转成文字描述,用于内容归档、检索、无障碍阅读。" }
                ],
                "md": "# 识图 · 图片转文字\n\n> ai-gateway 路由插件 · 官方 · 挂载点 media_parse\n\n## 功能简介\n\n上传/引用一张图片,调用上游 chat 模型带图识别,输出文字描述。\n\n## 应用场景\n\n### 给非多模态模型装上「眼睛」\n\nDeepSeek、GLM 等纯文本模型不能直接接收图片。本插件先把图片转成文字描述,再喂给模型——模型即可回答图片相关问题(截图理解、图表解读、商品图描述、验证码识别),原本非多模态的模型经此链路即获得识图能力。\n\n### 多模态链路中转站\n\n视频抽帧产物、设计稿、文档截图等,先经识图转成文字,再进入对话/总结/文档流程,全链路统一走文字通道,任何模型都能参与。\n\n### 批量图片描述\n\n媒体暂存里的一批图片,逐一转成文字描述,用于内容归档、检索、无障碍阅读。\n\n## 调用的模型能力\n\n调用上游 chat 通道(图片以 image_url 注入),模型默认取供应商当前默认,可指定。支持多模型故障转移:主模型失败自动切换备用模型。\n\n## 使用说明\n\n1. 在「供应商」里配置好支持识图的模型(能力面板可探测 image_in)\n2. 安装本插件,选择识别模型\n3. 调用入口:POST /api/plugins/:id/invoke,传入 media_url(网关暂存或本机路径)\n4. 输出文字描述,可继续喂给对话/文档流程"
            },
            {
                "id": "image-generate",
                "name": "文生图 · 文字生成图片",
                "author": "官方",
                "version": "2.0.0",
                "cap": "文生图",
                "icon": "🎨",
                "mount": "tool_exec",
                "builtin": true,
                "ui": true,
                "short_desc": "文字生成图片",
                "desc": "输入一句话描述,调用上游 /v1/images/generations 生成图片,产物自动存入媒体暂存。",
                "input": { "required": ["prompt"], "properties": { "prompt": "string", "model": "string", "provider_id": "string", "size": "string", "n": "number" } },
                "output": { "properties": { "media_urls": "string[]", "mime": "string" } },
                "timeout_ms": 240000,
                "models": [
                    { "id": "gpt-image-1", "api": "https://2xa.cc.cd/v1", "note": "主模型:图出(需上游开通权限)" }
                ],
                "config": [
                    { "k": "model", "label": "生成模型", "type": "select", "def": "gpt-image-1", "options": ["gpt-image-1"] },
                    { "k": "size", "label": "默认尺寸", "type": "select", "def": "1024×1024", "options": ["1024×1024", "512×512"] },
                    { "k": "n", "label": "一次生成张数", "type": "number", "def": 1 }
                ],
                "scenes": [
                    { "title": "文案配图", "desc": "写文章、推文、文档时,一句话生成配图,不用再找图库。" },
                    { "title": "设计素材", "desc": "快速生成示意图、插画、概念图,给设计/汇报提供视觉参考。" },
                    { "title": "创意发散", "desc": "同一描述生成多张方案,辅助头脑风暴与方案比选。" }
                ],
                "md": "# 文生图 · 文字生成图片\n\n> ai-gateway 路由插件 · 官方 · 挂载点 tool_exec\n\n## 功能简介\n\n输入一句话描述,调用上游 /v1/images/generations 生成图片,产物自动存入媒体暂存。\n\n## 应用场景\n\n### 文案配图\n\n写文章、推文、文档时,一句话生成配图,不用再找图库。\n\n### 设计素材\n\n快速生成示意图、插画、概念图,给设计/汇报提供视觉参考。\n\n### 创意发散\n\n同一描述生成多张方案,辅助头脑风暴与方案比选。\n\n## 调用的模型能力\n\n调用上游 /v1/images/generations(gpt-image 系模型),产物 b64_json/url 双形态入暂存。支持多模型故障转移。\n\n## 使用说明\n\n1. 需上游 Key 组开通图生成权限(未开通时调用返回人话提示)\n2. 生成产物自动入媒体暂存,返回管内 URL\n3. 可继续喂给识图/对话流程"
            },
            {
                "id": "image-edit",
                "name": "图编辑 · 图片指令修改",
                "author": "官方",
                "version": "1.1.0",
                "cap": "图编辑",
                "icon": "✂️",
                "mount": "tool_exec",
                "builtin": true,
                "ui": true,
                "short_desc": "按指令编辑图片",
                "desc": "原图 + 修改指令,调用上游 /v1/images/edits 生成新图,产物入暂存。",
                "input": { "required": ["media_url", "prompt"], "properties": { "media_url": "string", "prompt": "string", "model": "string", "provider_id": "string", "size": "string" } },
                "output": { "properties": { "media_url": "string", "mime": "string" } },
                "timeout_ms": 240000,
                "models": [
                    { "id": "gpt-image-1", "api": "https://2xa.cc.cd/v1", "note": "主模型:图编辑(需上游开通权限)" }
                ],
                "config": [
                    { "k": "model", "label": "编辑模型", "type": "select", "def": "gpt-image-1", "options": ["gpt-image-1"] }
                ],
                "scenes": [
                    { "title": "图片修改", "desc": "对现有图片按指令修改:换风格、改元素、扩场景,产物入暂存继续处理。" },
                    { "title": "多轮迭代", "desc": "生成不满意?给原图加新指令再改,配合文生图形成「生成→修改→定稿」闭环。" }
                ],
                "md": "# 图编辑 · 图片指令修改\n\n> ai-gateway 路由插件 · 官方 · 挂载点 tool_exec\n\n## 功能简介\n\n原图 + 修改指令,调用上游 /v1/images/edits 生成新图,产物入暂存。\n\n## 应用场景\n\n### 图片修改\n\n对现有图片按指令修改:换风格、改元素、扩场景,产物入暂存继续处理。\n\n### 多轮迭代\n\n生成不满意?给原图加新指令再改,配合文生图形成「生成→修改→定稿」闭环。\n\n## 调用的模型能力\n\n调用上游 /v1/images/edits(multipart 原图 + 指令),新图入媒体暂存。支持多模型故障转移。\n\n## 使用说明\n\n1. 需上游 Key 组开通图生成权限\n2. 原图从网关暂存或本机路径读取\n3. 产物入暂存,可继续处理"
            },
            {
                "id": "asr-speech",
                "name": "语音识别 · 音频转文字",
                "author": "官方",
                "version": "1.2.0",
                "cap": "ASR",
                "icon": "🎙️",
                "mount": "media_parse",
                "builtin": true,
                "ui": true,
                "short_desc": "音频转文字",
                "desc": "上传一段音频,识别为文字。配置你自己的 ASR 服务端点即可使用(Whisper 形态,兼容 OpenAI / Azure / 中转站等)。",
                "input": { "required": ["media_url"], "properties": { "media_url": "string" } },
                "output": { "properties": { "text": "string" } },
                "timeout_ms": 120000,
                "models": [
                    { "id": "whisper-1", "api": "", "note": "主模型:默认端点留空,请在配置页填入你的 ASR 服务端点" }
                ],
                "config": [
                    { "k": "apiBase", "label": "ASR API 地址", "type": "text", "def": "", "hint": "任意提供 ASR 的服务(OpenAI Whisper / Azure 语音 / 中转站),默认留空,调用返回人话引导配置" },
                    { "k": "apiKey", "label": "API Key", "type": "password", "req": true, "hint": "你的服务商 Key" },
                    { "k": "model", "label": "识别模型", "type": "select", "def": "whisper-1", "options": ["whisper-1", "whisper-large-v3", "其他"] }
                ],
                "scenes": [
                    { "title": "语音转文字", "desc": "会议录音、语音消息、采访素材转成文字,进入对话/总结/记录流程。配置你自己的 ASR 端点即可用。" }
                ],
                "md": "# 语音识别 · 音频转文字\n\n> ai-gateway 路由插件 · 官方 · 挂载点 media_parse\n\n## 功能简介\n\n上传一段音频,识别为文字。配置你自己的 ASR 服务端点即可使用(Whisper 形态,兼容 OpenAI / Azure / 各类中转站)。\n\n## 应用场景\n\n### 语音转文字\n\n会议录音、语音消息、采访素材转成文字,进入对话/总结/记录流程。\n\n## 调用的模型能力\n\n调用配置的 ASR 端点(Whisper 形态);支持多端点故障转移,主端点失败自动切换备用。\n\n## 使用说明\n\n1. 配置你自己的 ASR 服务端点(API 地址 + Key + 模型)\n2. 安装后即可识别音频为文字\n3. 可配置多个备用端点,主端点失败自动切换(故障转移)\n4. 产物文字可继续喂给对话/总结流程"
            },
            {
                "id": "tts-speech",
                "name": "语音合成 · 文字转语音",
                "author": "官方",
                "version": "1.2.0",
                "cap": "TTS",
                "icon": "🔊",
                "mount": "tool_exec",
                "builtin": true,
                "ui": true,
                "short_desc": "文字转语音",
                "desc": "输入文字,合成为语音。配置你自己的 TTS 服务端点即可使用,支持多音色。",
                "input": { "required": ["text"], "properties": { "text": "string" } },
                "output": { "properties": { "media_url": "string", "mime": "string" } },
                "timeout_ms": 120000,
                "models": [
                    { "id": "tts-1", "api": "", "note": "主模型:默认端点留空,请在配置页填入你的 TTS 服务端点" }
                ],
                "config": [
                    { "k": "apiBase", "label": "TTS API 地址", "type": "text", "def": "", "hint": "任意提供 TTS 的服务(OpenAI / Azure / 中转站),默认留空,调用返回人话引导配置" },
                    { "k": "apiKey", "label": "API Key", "type": "password", "req": true, "hint": "你的服务商 Key" },
                    { "k": "model", "label": "合成模型", "type": "select", "def": "tts-1", "options": ["tts-1", "tts-1-hd", "gpt-4o-mini-tts", "其他"] },
                    { "k": "voice", "label": "默认音色", "type": "select", "def": "alloy", "options": ["alloy", "echo", "fable", "onyx", "nova", "shimmer"] }
                ],
                "scenes": [
                    { "title": "文字转语音", "desc": "文章、消息、通知转成语音播放,朗读与播报场景。配置你自己的 TTS 端点即可用。" }
                ],
                "md": "# 语音合成 · 文字转语音\n\n> ai-gateway 路由插件 · 官方 · 挂载点 tool_exec\n\n## 功能简介\n\n输入文字,合成为语音。配置你自己的 TTS 服务端点即可使用,支持多音色。\n\n## 应用场景\n\n### 文字转语音\n\n文章、消息、通知转成语音播放,朗读与播报场景。\n\n## 调用的模型能力\n\n调用配置的 TTS 端点;支持多端点故障转移,主端点失败自动切换备用。\n\n## 使用说明\n\n1. 配置你自己的 TTS 服务端点(API 地址 + Key + 音色)\n2. 安装后即可文字转语音\n3. 可配置多个备用端点,主端点失败自动切换(故障转移)\n4. 产物音频入暂存,可播放/下载"
            }
        ]
    })
}

fn sources_path(codex_home: &std::path::Path) -> std::path::PathBuf {
    codex_home.join("market-sources.json")
}

fn load_sources(codex_home: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(sources_path(codex_home))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v.get("sources").and_then(|s| s.as_array().cloned()))
        .unwrap_or_default()
}

#[allow(dead_code)]
fn save_sources(codex_home: &std::path::Path, sources: &[Value]) {
    let _ = save_sources_checked(codex_home, sources);
}

fn save_sources_checked(codex_home: &std::path::Path, sources: &[Value]) -> Result<(), String> {
    let p = sources_path(codex_home);
    let body = json!({ "version": 1, "sources": sources });
    let parent = p
        .parent()
        .ok_or_else(|| "插件源文件缺少父目录".to_string())?;
    std::fs::create_dir_all(parent).map_err(|e| format!("创建插件源目录失败: {e}"))?;
    let tmp = parent.join(format!(
        ".market-sources.{}.{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&tmp, body.to_string()).map_err(|e| format!("写插件源临时文件失败: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("设置插件源文件权限失败: {e}"))?;
    }
    #[cfg(windows)]
    if p.exists() {
        let old = parent.join(format!(
            ".market-sources-old.{}.{}.bak",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::rename(&p, &old).map_err(|e| format!("替换旧插件源文件失败: {e}"))?;
        if let Err(error) = std::fs::rename(&tmp, &p) {
            let _ = std::fs::rename(&old, &p);
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("替换插件源文件失败: {error}"));
        }
        let _ = std::fs::remove_file(old);
    }
    #[cfg(windows)]
    if !p.exists() {
        if let Err(error) = std::fs::rename(&tmp, &p) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("替换插件源文件失败: {error}"));
        }
    }
    #[cfg(not(windows))]
    std::fs::rename(&tmp, &p).map_err(|e| format!("替换插件源文件失败: {e}"))?;
    Ok(())
}

/// 拉取第三方源清单并校验(M5:schema_version 须识别;失败拒收整源)。
pub async fn fetch_market(url: &str) -> Result<Value, String> {
    let resp = plugin_client()
        .get(url.trim_end_matches('/'))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("清单拉取失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("清单拉取失败(HTTP {})", resp.status()));
    }
    let v: Value = resp.json().await.map_err(|e| format!("清单非 JSON: {e}"))?;
    if v.get("schema_version").and_then(|x| x.as_u64()) != Some(1) {
        return Err("schema_version 不识别(须为 1),整源拒收".into());
    }
    if v.get("plugins").and_then(|p| p.as_array()).is_none() {
        return Err("清单缺 plugins 数组".into());
    }
    Ok(v)
}

// ── 内置抽帧 tool(官方转正:注册表 tool 条目,invoke 分发本机 ffmpeg)──

/// 从媒体暂存 URL / 本地路径抽一帧。ffmpeg 缺失/失败 → 200 包错误(插件契约同形)。
#[cfg(test)]
async fn builtin_frame_extract(codex_home: &std::path::Path, body: &Value) -> Response {
    builtin_frame_extract_config(codex_home, body, None).await
}

fn numeric_value(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse::<f64>().ok())
}

async fn builtin_frame_extract_config(
    codex_home: &std::path::Path,
    body: &Value,
    config: Option<&Map<String, Value>>,
) -> Response {
    let Some(media_url) = body
        .get("media_url")
        .and_then(|v| v.as_str())
        .map(str::trim)
    else {
        return raw_json(
            StatusCode::OK,
            &json!({"ok": false, "error": {"code": "E_ARGS", "message": "media_url 必填", "human": "请提供视频的媒体地址"}}),
        );
    };
    let t = body
        .get("t")
        .and_then(numeric_value)
        .or_else(|| config?.get("defaultT").and_then(numeric_value))
        .unwrap_or(0.0);
    let ffmpeg_path = body_text(body, "ffmpegPath")
        .or_else(|| config.and_then(|values| config_text(values, "ffmpegPath")))
        .unwrap_or("ffmpeg")
        .to_string();
    let src = match resolve_media_path(codex_home, media_url, MAX_VIDEO_INPUT_BYTES) {
        Ok(path) => path.to_string_lossy().into_owned(),
        Err((code, message)) => {
            return raw_json(
                StatusCode::OK,
                &json!({"ok": false, "error": {"code": code, "message": message, "human": "请先上传视频并使用网关暂存地址"}}),
            );
        }
    };
    let home = codex_home.to_path_buf();
    let extracted = tokio::task::spawn_blocking(move || {
        std::process::Command::new(ffmpeg_path)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-ss",
                &format!("{t}"),
                "-i",
                &src,
                "-frames:v",
                "1",
                "-f",
                "image2",
                "-",
            ])
            .output()
    })
    .await;
    let out = match extracted {
        Ok(Ok(o)) if o.status.success() => o.stdout,
        Ok(Ok(o)) => {
            return raw_json(
                StatusCode::OK,
                &json!({"ok": false, "error": {"code": "E_MEDIA_FFMPEG", "message": String::from_utf8_lossy(&o.stderr).chars().take(200).collect::<String>(), "human": "ffmpeg 抽帧失败,请确认视频文件可解码"}}),
            );
        }
        Ok(Err(e)) => {
            return raw_json(
                StatusCode::OK,
                &json!({"ok": false, "error": {"code": "E_MEDIA_FFMPEG", "message": e.to_string(), "human": "本机未安装 ffmpeg 或无法执行,请安装 ffmpeg(如 brew install ffmpeg)"}}),
            );
        }
        Err(e) => {
            return err_env(
                StatusCode::INTERNAL_SERVER_ERROR,
                "E_INTERNAL",
                &format!("内部执行失败: {e}"),
            );
        }
    };
    match crate::media::store_upload(&home, &out, "image/jpeg", "ffmpeg-frame-extract") {
        Ok(item) => raw_json(
            StatusCode::OK,
            &json!({"ok": true, "data": {"media_url": format!("/media/{}.{}", item.id, item.ext), "mime": item.mime}}),
        ),
        Err((code, msg)) => raw_json(
            StatusCode::OK,
            &json!({"ok": false, "error": {"code": code, "message": msg, "human": "抽帧结果存储失败"}}),
        ),
    }
}

// ── 市场 handlers ────────────────────────────────────────────

async fn handle_market_list(State(s): State<Arc<crate::server::AppState>>) -> Response {
    let mut sources = vec![json!({ "id": OFFICIAL_SOURCE, "name": "官方源", "builtin": true })];
    sources.extend(load_sources(&s.codex_home));
    raw_json(
        StatusCode::OK,
        &json!({ "sources": sources, "official": official_market() }),
    )
}

/// 按源拉插件清单(遗留④闭环):官方源→内置清单;第三方源→实时拉取校验(M5 同口径)。
async fn handle_source_plugins(
    State(s): State<Arc<crate::server::AppState>>,
    Path(id): Path<String>,
) -> Response {
    if id == OFFICIAL_SOURCE {
        return raw_json(
            StatusCode::OK,
            &json!({ "source_id": OFFICIAL_SOURCE, "plugins": official_market()["plugins"] }),
        );
    }
    let Some(url) = load_sources(&s.codex_home)
        .into_iter()
        .find(|x| x["id"] == id.as_str())
        .and_then(|x| x["url"].as_str().map(String::from))
    else {
        return err_env(StatusCode::NOT_FOUND, "E_NO_SOURCE", "源不存在,请先添加");
    };
    match fetch_market(&url).await {
        Ok(m) => raw_json(
            StatusCode::OK,
            &json!({ "source_id": id, "plugins": m["plugins"] }),
        ),
        Err(e) => err_env(StatusCode::BAD_GATEWAY, "E_SOURCE_FETCH", &e),
    }
}

async fn handle_market_source_add(
    State(s): State<Arc<crate::server::AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let id = body
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let url = body
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if id.is_empty()
        || name.is_empty()
        || !(url.starts_with("http://") || url.starts_with("https://"))
    {
        return err_env(
            StatusCode::BAD_REQUEST,
            "E_ARGS",
            "id/name/url 必填(url 须 http(s) 清单地址)",
        );
    }
    if id == OFFICIAL_SOURCE
        || load_sources(&s.codex_home)
            .iter()
            .any(|x| x["id"] == id.as_str())
    {
        return err_env(StatusCode::BAD_REQUEST, "E_SOURCE_DUP", "源 id 已存在");
    }
    match fetch_market(&url).await {
        Ok(_) => {
            let mut sources = load_sources(&s.codex_home);
            sources.push(json!({ "id": id, "name": name, "url": url }));
            match save_sources_checked(&s.codex_home, &sources) {
                Ok(()) => raw_json(StatusCode::OK, &json!({ "ok": true, "id": id })),
                Err(error) => err_env(StatusCode::INTERNAL_SERVER_ERROR, "E_SOURCE_SAVE", &error),
            }
        }
        Err(e) => err_env(StatusCode::BAD_REQUEST, "E_SOURCE_FETCH", &e),
    }
}

async fn handle_market_source_remove(
    State(s): State<Arc<crate::server::AppState>>,
    Path(id): Path<String>,
) -> Response {
    if id == OFFICIAL_SOURCE {
        return err_env(StatusCode::FORBIDDEN, "E_OFFICIAL", "官方源不可删除");
    }
    let mut sources = load_sources(&s.codex_home);
    let before = sources.len();
    sources.retain(|x| x["id"] != id.as_str());
    match save_sources_checked(&s.codex_home, &sources) {
        Ok(()) => raw_json(
            StatusCode::OK,
            &json!({ "ok": true, "removed": before != sources.len() }),
        ),
        Err(error) => err_env(StatusCode::INTERNAL_SERVER_ERROR, "E_SOURCE_SAVE", &error),
    }
}

/// 安装:官方源→内置条目直接登记;第三方源→拉清单→逐条目 manifest 校验→登记。
async fn handle_market_install(
    State(s): State<Arc<crate::server::AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let source_id = body
        .get("sourceId")
        .and_then(|v| v.as_str())
        .unwrap_or(OFFICIAL_SOURCE)
        .to_string();
    let plugin_id = body
        .get("pluginId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if plugin_id.is_empty() {
        return err_env(StatusCode::BAD_REQUEST, "E_ARGS", "pluginId 必填");
    }
    let manifest: Map<String, Value> = if source_id == OFFICIAL_SOURCE {
        match official_market()["plugins"]
            .as_array()
            .and_then(|a| a.iter().find(|p| p["id"] == plugin_id.as_str()).cloned())
            .and_then(|p| p.as_object().cloned())
        {
            Some(m) => m,
            None => return err_env(StatusCode::NOT_FOUND, "E_NO_PLUGIN", "官方源无该插件"),
        }
    } else {
        let Some(src) = load_sources(&s.codex_home)
            .into_iter()
            .find(|x| x["id"] == source_id.as_str())
            .and_then(|x| x["url"].as_str().map(String::from))
        else {
            return err_env(StatusCode::NOT_FOUND, "E_NO_SOURCE", "源不存在,请先添加");
        };
        let market = match fetch_market(&src).await {
            Ok(v) => v,
            Err(e) => return err_env(StatusCode::BAD_REQUEST, "E_SOURCE_FETCH", &e),
        };
        let Some(entry) = market["plugins"]
            .as_array()
            .and_then(|a| a.iter().find(|p| p["id"] == plugin_id.as_str()).cloned())
        else {
            return err_env(StatusCode::NOT_FOUND, "E_NO_PLUGIN", "该源无此插件");
        };
        let Some(endpoint) = entry.get("endpoint").and_then(|v| v.as_str()) else {
            return err_env(
                StatusCode::BAD_REQUEST,
                "E_PLUGIN_MANIFEST",
                "清单条目缺 endpoint",
            );
        };
        let mut m = match fetch_manifest(endpoint).await {
            Ok(m) => m,
            Err(e) => return err_env(StatusCode::BAD_REQUEST, "E_PLUGIN_MANIFEST", &e),
        };
        if m.get("id").and_then(Value::as_str) != Some(plugin_id.as_str()) {
            return err_env(
                StatusCode::BAD_REQUEST,
                "E_PLUGIN_MANIFEST",
                "远程 manifest id 与所选插件不一致,拒绝安装",
            );
        }
        m.insert("source_id".into(), json!(source_id));
        m
    };
    // 内置 tool 条目做 manifest 校验;http 型已在 fetch_manifest 内校验
    if manifest
        .get("builtin")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        if let Err(e) = validate_manifest(&manifest) {
            return err_env(StatusCode::BAD_REQUEST, "E_PLUGIN_MANIFEST", &e);
        }
    }
    let pid = manifest
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    registry::upsert_plugin(&s.codex_home, &manifest);
    raw_json(StatusCode::OK, &json!({ "ok": true, "id": pid }))
}

#[cfg(test)]
mod market_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let h = std::env::temp_dir().join(format!("2xapi-mkt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&h);
        std::fs::create_dir_all(&h).unwrap();
        h
    }

    #[test]
    fn official_install_creates_tool_entry() {
        let home = tmp_home("kind");
        let entry_manifest = official_market()["plugins"][0]
            .as_object()
            .cloned()
            .unwrap();
        validate_manifest(&entry_manifest).unwrap();
        registry::upsert_plugin(&home, &entry_manifest);
        let e = registry::get_plugin(&home, "ffmpeg-frame-extract").unwrap();
        assert_eq!(e.kind, registry::Kind::Tool, "内置能力=tool 条目");
        assert!(e.enabled);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// install handler:官方条目安装 version 以市场为准;旧版登记幂等安装同步市场版;未上架 404。
    #[tokio::test]
    async fn install_uses_market_version_and_syncs_stale() {
        let root = tmp_home("install-v");
        let s = std::sync::Arc::new(mk_state(&root));
        let resp = handle_install(State(s.clone()), Path("image-describe".into())).await;
        let v = body_of(resp).await;
        assert_eq!(v["ok"], true, "{v}");
        let e = registry::get_plugin(&s.codex_home, "image-describe").unwrap();
        assert_eq!(e.meta["version"], "2.0.1", "version 应以市场为准");
        assert!(e.enabled);

        // 旧版登记(1.0.0)→ 幂等安装同步市场版
        let root2 = tmp_home("install-stale");
        let s2 = std::sync::Arc::new(mk_state(&root2));
        let mut stale = official_market()["plugins"][0]
            .as_object()
            .cloned()
            .unwrap();
        stale.insert("version".into(), json!("1.0.0"));
        registry::upsert_plugin(&s2.codex_home, &stale);
        let resp2 = handle_install(State(s2.clone()), Path("ffmpeg-frame-extract".into())).await;
        assert_eq!(body_of(resp2).await["ok"], true);
        let e2 = registry::get_plugin(&s2.codex_home, "ffmpeg-frame-extract").unwrap();
        assert_eq!(e2.meta["version"], "1.0.1", "市场 ffmpeg 版应同步到最新版");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&root2);
    }

    /// ffmpeg 端到端(本机无 ffmpeg 时验证人话错误分支)。
    #[tokio::test]
    async fn builtin_frame_extract_end_to_end() {
        let home = tmp_home("ffmpeg");
        let has_ffmpeg = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-version"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !has_ffmpeg {
            let resp = builtin_frame_extract(&home, &json!({"media_url": "/media/x.mp4"})).await;
            let v: Value = serde_json::from_slice(
                &axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(v["ok"], false);
            assert_eq!(
                v["error"]["code"], "E_MEDIA_NOT_FOUND",
                "缺 ffmpeg 机器也应先过暂存校验"
            );
            let _ = std::fs::remove_dir_all(&home);
            return;
        }
        // ffmpeg testsrc 生成 1s mp4 → 入暂存 → invoke 抽帧
        let mp4 = std::env::temp_dir().join(format!("2xapi-fe-{}.mp4", std::process::id()));
        let gen = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=128x96:rate=10",
                "-y",
            ])
            .arg(&mp4)
            .output()
            .unwrap();
        assert!(gen.status.success(), "生成测试视频失败: {:?}", gen.stderr);
        let data = std::fs::read(&mp4).unwrap();
        let item = crate::media::store_upload(&home, &data, "video/mp4", "test").unwrap();
        let resp = builtin_frame_extract(
            &home,
            &json!({"media_url": format!("http://127.0.0.1:8787/media/{}.{}", item.id, item.ext), "t": 0.5}),
        )
        .await;
        let v: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v["ok"], true, "抽帧应成功: {v}");
        let url = v["data"]["media_url"].as_str().unwrap();
        assert!(url.starts_with("/media/") && url.ends_with(".jpg"), "{url}");
        // 产物落暂存可读
        let id_ext = url.trim_start_matches("/media/");
        let out = std::fs::read(crate::media::media_root(&home).join(id_ext)).unwrap();
        assert_eq!(&out[..3], b"\xFF\xD8\xFF", "JPEG magic");
        let _ = std::fs::remove_file(&mp4);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 坏输入两形态:media_url 缺失 / 非 hex id 路径穿越拒绝。
    #[tokio::test]
    async fn builtin_frame_extract_bad_input() {
        let home = tmp_home("bad");
        for (body, code) in [
            (json!({}), "E_ARGS"),
            (
                json!({"media_url": "/media/../../etc/passwd.mp4"}),
                "E_ARGS",
            ),
        ] {
            let resp = builtin_frame_extract(&home, &body).await;
            let v: Value = serde_json::from_slice(
                &axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(v["error"]["code"], code, "{body}");
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn builtin_frame_extract_honors_config_and_body_overrides() {
        use std::os::unix::fs::PermissionsExt;

        let home = tmp_home("ffmpeg-config");
        let media_root = crate::media::media_root(&home);
        std::fs::create_dir_all(&media_root).unwrap();
        let input = media_root.join("feedface.mp4");
        let script = home.join("fake-ffmpeg.sh");
        let args_log = home.join("ffmpeg-args.txt");
        std::fs::write(&input, b"video fixture").unwrap();
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\nprintf '\\377\\330\\377\\331'\n",
                args_log.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script, permissions).unwrap();

        let config: Map<String, Value> = serde_json::from_value(json!({
            "ffmpegPath": script.to_string_lossy(),
            "defaultT": "2.75"
        }))
        .unwrap();
        let value = body_of(
            builtin_frame_extract_config(
                &home,
                &json!({"media_url": input.to_string_lossy()}),
                Some(&config),
            )
            .await,
        )
        .await;
        assert_eq!(value["ok"], true, "配置的 ffmpegPath 应被执行: {value}");
        let args = std::fs::read_to_string(&args_log).unwrap();
        assert!(args
            .lines()
            .collect::<Vec<_>>()
            .windows(2)
            .any(|pair| pair == ["-ss", "2.75"]));

        let override_config: Map<String, Value> = serde_json::from_value(json!({
            "ffmpegPath": "/definitely/missing/ffmpeg",
            "defaultT": 9
        }))
        .unwrap();
        let value = body_of(
            builtin_frame_extract_config(
                &home,
                &json!({
                    "media_url": input.to_string_lossy(),
                    "ffmpegPath": script.to_string_lossy(),
                    "t": 1.25
                }),
                Some(&override_config),
            )
            .await,
        )
        .await;
        assert_eq!(value["ok"], true, "body 显式配置应优先: {value}");
        let args = std::fs::read_to_string(&args_log).unwrap();
        assert!(args
            .lines()
            .collect::<Vec<_>>()
            .windows(2)
            .any(|pair| pair == ["-ss", "1.25"]));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 市场清单 schema 校验(mock 清单源:合法/坏 schema_version 两形态)。
    #[tokio::test]
    async fn fetch_market_schema_validation() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        async fn serve(body: &'static str) -> String {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        break;
                    };
                    let body = body;
                    tokio::spawn(async move {
                        let mut buf = [0u8; 1024];
                        let _ = sock.read(&mut buf).await;
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(), body
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                    });
                }
            });
            format!("http://{addr}/list.json")
        }
        let good = serve(r#"{"schema_version":1,"source":{"id":"s1"},"plugins":[{"id":"p","endpoint":"http://x"}]}"#).await;
        let v = fetch_market(&good).await.unwrap();
        assert_eq!(v["plugins"][0]["id"], "p");
        let bad = serve(r#"{"schema_version":2,"plugins":[]}"#).await;
        assert!(
            fetch_market(&bad).await.is_err(),
            "schema_version 不识别整源拒收"
        );
    }

    // ── v3:manifest v2 校验 / 本地添加 / 配置往返 / 启停 / 更新 / 故障转移 / 官方 6 条 ──

    fn mk_state(root: &std::path::Path) -> crate::server::AppState {
        crate::server::AppState {
            config_path: root.join("config.toml"),
            backup_dir: root.join("backups"),
            providers_path: root.join("providers.json"),
            codex_home: root.join("codex"),
            wb_home: root.to_path_buf(),
            hermes_home: root.join("hermes"),
            gem_home: root.to_path_buf(),
            grok_home: root.join("grok"),
            oc_home: root.to_path_buf(),
            oclaw_home: root.join("oclaw"),
            cd_home: root.to_path_buf(),
            cursor_home: root.join("cursorhome"),
            launcher: Default::default(),
            health: std::sync::Arc::new(crate::acclines::HealthState::new(vec![])),
            accel: std::sync::Arc::new(std::sync::Mutex::new(crate::server::AccelCfg::default())),
            nodecreds: std::sync::Arc::new(
                std::sync::RwLock::new(crate::nodecreds::Store::empty()),
            ),
            keypool: std::sync::Arc::new(crate::keypool::KeyPool::new()),
            tray_gate_enabled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    /// mock http 服务:按 path 子串路由返回 status+body;body 内 "__ADDR__" 替换为实际地址。
    async fn mock_http(routes: Vec<(String, u16, String)>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let addr_s = addr.to_string();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let routes = routes.clone();
                let addr_s = addr_s.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    let _ = sock.read(&mut buf).await;
                    let req = String::from_utf8_lossy(&buf).into_owned();
                    let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
                    let (status, mut body) = routes
                        .iter()
                        .find(|(p, _, _)| path.contains(p.as_str()))
                        .map(|(_, s, b)| (*s, b.clone()))
                        .unwrap_or((404, "{}".into()));
                    body = body.replace("__ADDR__", &addr_s);
                    let reason = match status {
                        200 => "OK",
                        500 => "Internal Server Error",
                        _ => "Not Found",
                    };
                    let resp = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    async fn body_of(r: Response) -> Value {
        let b = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&b).unwrap()
    }

    #[tokio::test]
    async fn market_install_rejects_manifest_id_mismatch() {
        let root = tmp_home("manifest-id-mismatch");
        let state = std::sync::Arc::new(mk_state(&root));
        std::fs::create_dir_all(&state.codex_home).unwrap();
        let server = mock_http(vec![
            (
                "/market".into(),
                200,
                r#"{"schema_version":1,"plugins":[{"id":"wanted-plugin","endpoint":"http://__ADDR__/plugin"}]}"#.into(),
            ),
            (
                "/plugin/manifest".into(),
                200,
                r#"{"id":"different-plugin","name":"Different","version":"1.0.0","mount":"tool_exec","input":{},"output":{}}"#.into(),
            ),
        ])
        .await;
        save_sources(
            &state.codex_home,
            &[json!({"id":"remote-source","name":"Remote","url":format!("{server}/market")})],
        );

        let response = handle_market_install(
            State(state.clone()),
            Json(json!({"sourceId":"remote-source","pluginId":"wanted-plugin"})),
        )
        .await;
        let value = body_of(response).await;
        assert_eq!(value["ok"], false, "manifest id 不一致必须拒绝: {value}");
        assert_eq!(value["error"]["code"], "E_PLUGIN_MANIFEST");
        assert!(
            registry::get_plugin(&state.codex_home, "remote-source.different-plugin").is_none()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[derive(Debug)]
    struct CapturedRequest {
        head: String,
        body: Vec<u8>,
    }

    async fn mock_capture(
        status: u16,
        content_type: &str,
        response_body: Vec<u8>,
    ) -> (String, tokio::sync::oneshot::Receiver<CapturedRequest>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let content_type = content_type.to_string();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let (header_end, content_length) = loop {
                let mut chunk = [0u8; 4096];
                let read = socket.read(&mut chunk).await.unwrap();
                if read == 0 {
                    panic!("mock 请求未读到完整头部");
                }
                request.extend_from_slice(&chunk[..read]);
                let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let head = String::from_utf8_lossy(&request[..header_end]);
                let content_length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                break (header_end, content_length);
            };
            let body_start = header_end + 4;
            while request.len() < body_start + content_length {
                let mut chunk = [0u8; 4096];
                let read = socket.read(&mut chunk).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            let captured = CapturedRequest {
                head: String::from_utf8_lossy(&request[..header_end]).into_owned(),
                body: request[body_start..request.len().min(body_start + content_length)].to_vec(),
            };
            let reason = match status {
                200 => "OK",
                400 => "Bad Request",
                500 => "Internal Server Error",
                _ => "Response",
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response_body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.write_all(&response_body).await.unwrap();
            let _ = sender.send(captured);
        });
        (format!("http://{addr}"), receiver)
    }

    fn entry_with_models(models: Value, failover: bool) -> registry::Entry {
        let mut meta = Map::new();
        meta.insert("timeout_ms".into(), json!(2000));
        let mut config = Map::new();
        config.insert("models".into(), models);
        config.insert("failover".into(), json!(failover));
        registry::Entry {
            id: "t".into(),
            kind: registry::Kind::Plugin,
            provider_id: None,
            model: None,
            enabled: true,
            meta,
            config,
            source: "local".into(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn speech_security_checks_and_same_api_model_failover() {
        assert!(validate_keyed_endpoint("https://speech.example/v1").is_ok());
        assert!(validate_keyed_endpoint("http://127.0.0.1:8787/v1").is_ok());
        assert!(validate_keyed_endpoint("http://speech.example/v1").is_err());

        let entry = entry_with_models(
            json!([
                {"id":"whisper-a","api":"https://speech.example/v1"},
                {"id":"whisper-b","api":"https://speech.example/v1"}
            ]),
            true,
        );
        let endpoints = speech_endpoints(&entry, &json!({}), &Map::new(), "whisper-1");
        assert_eq!(
            endpoints.len(),
            2,
            "同一 API 地址的不同模型都必须保留用于故障转移"
        );
        assert_eq!(endpoints[0].model, "whisper-a");
        assert_eq!(endpoints[1].model, "whisper-b");
    }

    #[test]
    fn plugin_media_input_is_staging_only_and_size_limited() {
        let home = tmp_home("media-scope");
        let media_root = crate::media::media_root(&home);
        std::fs::create_dir_all(&media_root).unwrap();
        let managed = media_root.join("aaaaaaaa.wav");
        std::fs::write(&managed, b"RIFF0000WAVEfixture").unwrap();
        assert!(resolve_media_path(&home, "/media/aaaaaaaa.wav", MAX_ASR_INPUT_BYTES).is_ok());

        let outside = home.join("outside.wav");
        std::fs::write(&outside, b"RIFF0000WAVEoutside").unwrap();
        assert!(
            resolve_media_path(
                &home,
                outside.to_string_lossy().as_ref(),
                MAX_ASR_INPUT_BYTES
            )
            .is_err(),
            "暂存目录外绝对路径必须拒绝"
        );

        let oversized = media_root.join("bbbbbbbb.wav");
        let file = std::fs::File::create(&oversized).unwrap();
        file.set_len(MAX_ASR_INPUT_BYTES + 1).unwrap();
        assert!(
            resolve_media_path(&home, "/media/bbbbbbbb.wav", MAX_ASR_INPUT_BYTES).is_err(),
            "超限音频必须在读取前拒绝"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn builtin_asr_uses_standard_multipart_and_authorization() {
        let home = tmp_home("asr-standard");
        let media_root = crate::media::media_root(&home);
        std::fs::create_dir_all(&media_root).unwrap();
        let audio = media_root.join("aaaaaaaa.wav");
        std::fs::write(&audio, b"RIFF0000WAVEasr-fixture").unwrap();
        let (api, captured) = mock_capture(
            200,
            "application/json",
            r#"{"text":"识别成功"}"#.as_bytes().to_vec(),
        )
        .await;
        let entry = entry_with_models(json!([]), true);
        let config: Map<String, Value> = serde_json::from_value(json!({
            "apiBase": api,
            "apiKey": "asr-secret",
            "model": "whisper-custom"
        }))
        .unwrap();

        let response = builtin_asr_speech(
            &home,
            &entry,
            &json!({"media_url": audio.to_string_lossy()}),
            &config,
        )
        .await;
        let value = body_of(response).await;
        assert_eq!(value["ok"], true, "ASR 应返回插件成功信封: {value}");
        assert_eq!(value["data"]["text"], "识别成功");

        let request = tokio::time::timeout(std::time::Duration::from_secs(2), captured)
            .await
            .unwrap()
            .unwrap();
        let head = request.head.to_ascii_lowercase();
        assert!(
            head.starts_with("post /v1/audio/transcriptions http/1.1"),
            "应调用标准转写路径: {}",
            request.head
        );
        assert!(head.contains("authorization: bearer asr-secret"));
        assert!(head.contains("content-type: multipart/form-data; boundary="));
        let multipart = String::from_utf8_lossy(&request.body);
        assert!(multipart.contains("name=\"file\"; filename=\"aaaaaaaa.wav\""));
        assert!(multipart.contains("name=\"model\""));
        assert!(multipart.contains("whisper-custom"));
        assert!(
            multipart.contains("asr-fixture"),
            "multipart 应包含音频文件内容"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn builtin_tts_uses_standard_json_and_stores_audio() {
        let home = tmp_home("tts-standard");
        let audio = b"ID3\x04\0\0\0\0\0\0tts-fixture".to_vec();
        let (api, captured) = mock_capture(200, "audio/mpeg", audio.clone()).await;
        let entry = entry_with_models(json!([]), true);
        let config: Map<String, Value> = serde_json::from_value(json!({
            "apiBase": api,
            "apiKey": "tts-secret",
            "model": "tts-custom",
            "voice": "nova"
        }))
        .unwrap();

        let response =
            builtin_tts_speech(&home, &entry, &json!({"text": "你好，世界"}), &config).await;
        let value = body_of(response).await;
        assert_eq!(value["ok"], true, "TTS 应返回插件成功信封: {value}");
        assert_eq!(value["data"]["mime"], "audio/mpeg");
        let media_url = value["data"]["media_url"].as_str().unwrap();
        assert!(media_url.starts_with("/media/") && media_url.ends_with(".mp3"));
        assert_eq!(
            std::fs::read(
                crate::media::media_root(&home).join(media_url.trim_start_matches("/media/"))
            )
            .unwrap(),
            audio,
            "音频响应应原样进入媒体暂存"
        );

        let request = tokio::time::timeout(std::time::Duration::from_secs(2), captured)
            .await
            .unwrap()
            .unwrap();
        let head = request.head.to_ascii_lowercase();
        assert!(head.starts_with("post /v1/audio/speech http/1.1"));
        assert!(head.contains("authorization: bearer tts-secret"));
        assert!(head.contains("content-type: application/json"));
        let payload: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(payload["model"], "tts-custom");
        assert_eq!(payload["input"], "你好，世界");
        assert_eq!(payload["voice"], "nova");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn builtin_asr_fails_over_to_next_configured_endpoint() {
        let home = tmp_home("asr-failover");
        let media_root = crate::media::media_root(&home);
        std::fs::create_dir_all(&media_root).unwrap();
        let audio = media_root.join("bbbbbbbb.wav");
        std::fs::write(&audio, b"RIFF0000WAVEfailover").unwrap();
        let (primary, primary_request) = mock_capture(
            500,
            "application/json",
            br#"{"error":{"message":"down"}}"#.to_vec(),
        )
        .await;
        let (backup, backup_request) = mock_capture(
            200,
            "application/json",
            r#"{"text":"备用成功"}"#.as_bytes().to_vec(),
        )
        .await;
        let entry = entry_with_models(
            json!([
                {"id":"whisper-primary","api":primary},
                {"id":"whisper-backup","api":backup}
            ]),
            true,
        );
        let config: Map<String, Value> = serde_json::from_value(json!({
            "apiKey": "shared-secret"
        }))
        .unwrap();

        let value = body_of(
            builtin_asr_speech(
                &home,
                &entry,
                &json!({"media_url": audio.to_string_lossy()}),
                &config,
            )
            .await,
        )
        .await;
        assert_eq!(value["ok"], true, "主端点 500 后应切备用: {value}");
        assert_eq!(value["data"]["text"], "备用成功");

        for captured in [primary_request, backup_request] {
            let request = tokio::time::timeout(std::time::Duration::from_secs(2), captured)
                .await
                .unwrap()
                .unwrap();
            let head = request.head.to_ascii_lowercase();
            assert!(head.starts_with("post /v1/audio/transcriptions http/1.1"));
            assert!(head.contains("authorization: bearer shared-secret"));
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn manifest_v2_validation() {
        let mut m: Map<String, Value> = serde_json::from_value(json!({
            "id":"a-b","name":"n","version":"1.0.0","mount":"media_parse","input":{},"output":{},
            "models":[{"id":"m1","api":"http://x/v1","note":"主"}],
            "config":[{"k":"lang","label":"语言","type":"select","options":["中文"],"def":"中文"}],
            "scenes":[{"title":"t","desc":"d"}],
            "md":"# t","ui":true
        }))
        .unwrap();
        assert!(validate_manifest(&m).is_ok(), "合法 v2 应通过");
        // models 非法类型/缺 id 拒
        m.insert("models".into(), json!("m1"));
        assert!(validate_manifest(&m).is_err(), "models 非数组拒");
        m.insert("models".into(), json!([{ "api": "http://x" }]));
        assert!(validate_manifest(&m).is_err(), "models[0] 缺 id 拒");
        // config 非法类型/type 非枚举拒
        m.insert("models".into(), json!([{ "id": "m1" }]));
        m.insert("config".into(), json!([{ "k": "a", "type": "checkbox" }]));
        assert!(validate_manifest(&m).is_err(), "config type 非枚举拒");
        m.insert("config".into(), json!("nope"));
        assert!(validate_manifest(&m).is_err(), "config 非数组拒");
        // md 非字符串 / ui 非布尔拒
        m.insert("config".into(), json!([]));
        m.insert("md".into(), json!(123));
        assert!(validate_manifest(&m).is_err(), "md 非字符串拒");
        m.insert("md".into(), json!("x"));
        m.insert("ui".into(), json!("yes"));
        assert!(validate_manifest(&m).is_err(), "ui 非布尔拒");
        // 既有必填仍拒
        m.insert("ui".into(), json!(true));
        m.remove("mount");
        assert!(validate_manifest(&m).is_err(), "缺必填 mount 拒");
    }

    #[test]
    fn official_market_six_entries_full_fields() {
        let plugs = official_market()["plugins"].as_array().unwrap().clone();
        assert_eq!(plugs.len(), 6, "官方市场应 6 条");
        let ids: Vec<&str> = plugs.iter().map(|p| p["id"].as_str().unwrap()).collect();
        for want in [
            "ffmpeg-frame-extract",
            "image-describe",
            "image-generate",
            "image-edit",
            "asr-speech",
            "tts-speech",
        ] {
            assert!(ids.contains(&want), "缺 {want}");
        }
        for p in &plugs {
            validate_manifest(p.as_object().unwrap()).unwrap();
            assert!(
                p["md"].as_str().unwrap_or("").len() > 10,
                "md 应有内容: {}",
                p["id"]
            );
            assert!(
                p["scenes"]
                    .as_array()
                    .map(|a| !a.is_empty())
                    .unwrap_or(false),
                "scenes 非空: {}",
                p["id"]
            );
            assert!(p["config"].is_array(), "config: {}", p["id"]);
            assert!(p["models"].is_array(), "models: {}", p["id"]);
            assert!(p["ui"].as_bool().unwrap_or(false), "ui: {}", p["id"]);
        }
        // ASR/TTS 默认端点留空(提示用户配置)
        for id in ["asr-speech", "tts-speech"] {
            let p = plugs.iter().find(|x| x["id"] == id).unwrap();
            assert_eq!(p["models"][0]["api"], "", "{id} 默认端点应留空");
        }
    }

    #[tokio::test]
    async fn local_add_registers_with_source_prefix() {
        let root = tmp_home("local");
        let s = std::sync::Arc::new(mk_state(&root));
        let manifest = json!({
            "id":"my-describer","name":"我的识图增强","version":"1.0.0","author":"你",
            "mount":"media_parse",
            "input":{"required":["media_url"],"properties":{}},
            "output":{"properties":{"text":"string"}},
            "models":[{"id":"gpt-5.6","api":"https://2xa.cc.cd/v1","note":"主模型"}],
            "config":[{"k":"lang","label":"输出语言","type":"select","options":["中文"],"def":"中文"}],
            "scenes":[{"title":"t","desc":"d"}],
            "md":"# 我的识图增强\n## 功能简介","ui":true
        });
        let resp = handle_local_add(State(s.clone()), Json(manifest)).await;
        let v = body_of(resp).await;
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(
            v["id"], "local.my-describer",
            "本地添加 id 应加 local. 前缀"
        );
        let e = registry::get_plugin(&s.codex_home, "local.my-describer").unwrap();
        assert_eq!(e.source, "local");
        assert_eq!(e.kind, registry::Kind::Plugin);
        assert_eq!(e.config["failover"], true, "故障转移默认开");
        assert_eq!(
            e.config["values"]["lang"], "中文",
            "默认配置应从 manifest def 种子化"
        );
        assert_eq!(e.config["models"][0]["id"], "gpt-5.6");
        assert!(!e.updated_at.is_empty());
        // file 文本形态
        let resp = handle_local_add(
            State(s.clone()),
            Json(json!({ "file": r#"{"id":"p2","name":"P2","version":"1.0.0","mount":"tool_exec","input":{},"output":{}}"# })),
        )
        .await;
        assert_eq!(body_of(resp).await["id"], "local.p2");
        // 非法 manifest 拒
        let resp =
            handle_local_add(State(s.clone()), Json(json!({ "id": "x", "name": "n" }))).await;
        let v = body_of(resp).await;
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "E_PLUGIN_MANIFEST");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn config_detail_toggle_roundtrip() {
        let root = tmp_home("cfg");
        let s = std::sync::Arc::new(mk_state(&root));
        let m = official_market()["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "image-describe")
            .unwrap()
            .as_object()
            .cloned()
            .unwrap();
        registry::upsert_plugin(&s.codex_home, &m);
        // 保存配置:models 优先级 + failover + 配置项值
        let resp = handle_config(
            State(s.clone()),
            Path("image-describe".into()),
            Json(json!({
                "config": {"lang": "English"},
                "models": [
                    {"id":"m1","api":"http://a/v1","note":"主"},
                    {"id":"m2","api":"http://b/v1","note":"备"}
                ],
                "failover": true
            })),
        )
        .await;
        let v = body_of(resp).await;
        assert_eq!(v["ok"], true, "{v}");
        let e = registry::get_plugin(&s.codex_home, "image-describe").unwrap();
        assert_eq!(e.config["values"]["lang"], "English");
        assert_eq!(e.config["models"].as_array().unwrap().len(), 2);
        assert_eq!(e.config["failover"], true);
        assert!(!e.updated_at.is_empty());
        // 部分保存(只带 config)保留既有 models
        let _ = handle_config(
            State(s.clone()),
            Path("image-describe".into()),
            Json(json!({ "config": { "lang": "中文" } })),
        )
        .await;
        let e = registry::get_plugin(&s.codex_home, "image-describe").unwrap();
        assert_eq!(e.config["values"]["lang"], "中文");
        assert_eq!(
            e.config["models"].as_array().unwrap().len(),
            2,
            "缺 models 保存应保留既有优先级"
        );
        // 详情:md/scenes/config 全量 + 用户配置 + status/source
        let resp = handle_detail(State(s.clone()), Path("image-describe".into())).await;
        let d = body_of(resp).await;
        assert_eq!(d["data"]["id"], "image-describe");
        assert!(d["data"]["md"].as_str().unwrap().starts_with("# 识图"));
        assert!(d["data"]["scenes"].as_array().unwrap().len() >= 2);
        assert!(d["data"]["config"].is_array());
        assert_eq!(d["data"]["source"], "official");
        assert_eq!(d["data"]["status"], "enabled");
        assert_eq!(d["data"]["models"][0]["id"], "m1", "详情应返回用户优先级");
        assert_eq!(d["data"]["config_values"]["lang"], "中文");
        // 启停
        let resp = handle_toggle(
            State(s.clone()),
            Path("image-describe".into()),
            Json(json!({ "enabled": false })),
        )
        .await;
        let v = body_of(resp).await;
        assert_eq!(v["enabled"], false);
        assert!(
            !registry::get_plugin(&s.codex_home, "image-describe")
                .unwrap()
                .enabled
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn update_branches() {
        let root = tmp_home("upd");
        let s = std::sync::Arc::new(mk_state(&root));
        // 本地条目 → 400 人话
        let mut lm: Map<String, Value> = serde_json::from_value(json!({
            "id":"lp","name":"LP","version":"1.0.0","mount":"media_parse","input":{},"output":{}
        }))
        .unwrap();
        lm.insert("source".into(), json!("local"));
        lm.insert("source_id".into(), json!("local"));
        registry::upsert_plugin(&s.codex_home, &lm);
        let resp = handle_update(State(s.clone()), Path("local.lp".into())).await;
        let v = body_of(resp).await;
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "E_LOCAL_UPDATE");
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("本地插件更新请重新添加"));
        // 官方同版本 → updated:false
        let m = official_market()["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "image-describe")
            .unwrap()
            .as_object()
            .cloned()
            .unwrap();
        registry::upsert_plugin(&s.codex_home, &m);
        let resp = handle_update(State(s.clone()), Path("image-describe".into())).await;
        let v = body_of(resp).await;
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["updated"], false);
        assert_eq!(v["version"], "2.0.1");
        // 远程:清单+manifest mock,版本 1.0.0 → 2.0.0 更新成功;再更 updated:false
        let srv = mock_http(vec![
            (
                "/list.json".into(),
                200,
                r#"{"schema_version":1,"source":{"id":"s1"},"plugins":[{"id":"p","endpoint":"http://__ADDR__/p"}]}"#.into(),
            ),
            (
                "/p/manifest".into(),
                200,
                r#"{"id":"p","name":"P","version":"2.0.0","mount":"media_parse","input":{},"output":{},"endpoint":"http://__ADDR__/p"}"#.into(),
            ),
        ])
        .await;
        save_sources(
            &s.codex_home,
            &[json!({ "id": "s1", "name": "S", "url": format!("{srv}/list.json") })],
        );
        let mut m1: Map<String, Value> = serde_json::from_value(json!({
            "id":"p","name":"P","version":"1.0.0","mount":"media_parse","input":{},"output":{},
            "endpoint": format!("{srv}/p")
        }))
        .unwrap();
        m1.insert("source_id".into(), json!("s1"));
        registry::upsert_plugin(&s.codex_home, &m1);
        let resp = handle_update(State(s.clone()), Path("s1.p".into())).await;
        let v = body_of(resp).await;
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["updated"], true);
        assert_eq!(v["version"], "2.0.0");
        let resp = handle_update(State(s.clone()), Path("s1.p".into())).await;
        let v = body_of(resp).await;
        assert_eq!(v["updated"], false, "同版本不再更新: {v}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn failover_primary_500_switches_to_backup() {
        let a = mock_http(vec![(
            "/invoke".into(),
            500,
            r#"{"ok":false,"error":{"code":"E_500","message":"boom"}}"#.into(),
        )])
        .await;
        let b = mock_http(vec![(
            "/invoke".into(),
            200,
            r#"{"ok":true,"data":{"text":"ok"}}"#.into(),
        )])
        .await;
        let entry = entry_with_models(
            json!([
                {"id":"a","api":a,"note":"主"},
                {"id":"b","api":b,"note":"备"}
            ]),
            true,
        );
        let resp = invoke_with_failover(&entry, &json!({"op":"x"})).await;
        assert_eq!(resp.status(), StatusCode::OK, "主 500 应切备用");
        let v = body_of(resp).await;
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"]["text"], "ok");
    }

    #[tokio::test]
    async fn failover_all_fail_aggregates_human() {
        let a = mock_http(vec![(
            "/invoke".into(),
            500,
            r#"{"ok":false,"error":{"code":"E_500","message":"boom"}}"#.into(),
        )])
        .await;
        let b = mock_http(vec![(
            "/invoke".into(),
            200,
            r#"{"ok":false,"error":{"code":"E_X","message":"m","human":"业务不行"}}"#.into(),
        )])
        .await;
        let entry = entry_with_models(
            json!([
                {"id":"a","api":a,"note":"主"},
                {"id":"b","api":b,"note":"备"}
            ]),
            true,
        );
        let resp = invoke_with_failover(&entry, &json!({"op":"x"})).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let v = body_of(resp).await;
        assert_eq!(v["error"]["code"], "E_PLUGIN_FAILOVER");
        let human = v["error"]["message"].as_str().unwrap();
        assert!(human.contains("主模型 a"), "聚合应含主模型: {human}");
        assert!(human.contains("备用模型 b"), "聚合应含备用: {human}");
        assert!(human.contains("请检查配置或稍后重试"), "{human}");
    }

    #[tokio::test]
    async fn failover_empty_api_and_disabled_flag() {
        // 双空 api → 全败聚合含「未配置服务端点」
        let entry = entry_with_models(
            json!([
                {"id":"w","api":"","note":"主"},
                {"id":"x","api":"","note":"备"}
            ]),
            true,
        );
        let v = body_of(invoke_with_failover(&entry, &json!({"op":"x"})).await).await;
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("未配置服务端点"));
        // 单模型业务错透传(行为不变)
        let a = mock_http(vec![(
            "/invoke".into(),
            200,
            r#"{"ok":false,"error":{"code":"E_OP","message":"unknown","human":"未知操作"}}"#.into(),
        )])
        .await;
        let entry = entry_with_models(json!([{"id":"a","api":a,"note":"主"}]), true);
        let resp = invoke_with_failover(&entry, &json!({"op":"zzz"})).await;
        assert_eq!(resp.status(), StatusCode::OK, "单模型 200 包错误原样透传");
        let v = body_of(resp).await;
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["human"], "未知操作");
        // 关闭故障转移:多模型只试主模型(不再尝试备用)
        let a = mock_http(vec![(
            "/invoke".into(),
            500,
            r#"{"ok":false,"error":{"code":"E_500","message":"boom"}}"#.into(),
        )])
        .await;
        let b = mock_http(vec![(
            "/invoke".into(),
            200,
            r#"{"ok":true,"data":{"text":"ok"}}"#.into(),
        )])
        .await;
        let entry = entry_with_models(
            json!([
                {"id":"a","api":a,"note":"主"},
                {"id":"b","api":b,"note":"备"}
            ]),
            false,
        );
        let v = body_of(invoke_with_failover(&entry, &json!({"op":"x"})).await).await;
        assert_eq!(v["ok"], false);
        assert!(
            v["error"]["message"].as_str().unwrap().contains("主模型 a"),
            "关故障转移只试主模型: {v}"
        );
        assert!(
            !v["error"]["message"].as_str().unwrap().contains("备用"),
            "不应尝试备用: {v}"
        );
    }

    /// 内置媒体工具故障转移:models 配置下按优先级逐个尝试(主失败切备用,注入 model/api),
    /// 全败聚合人话;未配置 models → 原行为(active 供应商直连)。
    #[tokio::test]
    async fn builtin_media_failover_chain() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        // mock chat 上游:按请求体 model 分流(bad → 500,good → 成功)
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_c = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let seen_c = seen_c.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 8192];
                    loop {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                buf.extend_from_slice(&tmp[..n]);
                                let s = String::from_utf8_lossy(&buf);
                                if let Some(i) = s.find("\r\n\r\n") {
                                    let cl = s[..i]
                                        .lines()
                                        .find(|l| l.to_lowercase().starts_with("content-length:"))
                                        .and_then(|l| l.split(':').nth(1))
                                        .and_then(|v| v.trim().parse::<usize>().ok())
                                        .unwrap_or(0);
                                    if buf.len() >= i + 4 + cl {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    let req = String::from_utf8_lossy(&buf).into_owned();
                    seen_c.lock().unwrap().push(req.clone());
                    let (status, body) = if req.contains("\"bad\"") {
                        (500, r#"{"error":{"message":"boom"}}"#.to_string())
                    } else {
                        (
                            200,
                            r#"{"choices":[{"message":{"content":"红色"},"finish_reason":"stop"}]}"#
                                .to_string(),
                        )
                    };
                    let resp = format!(
                        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        if status == 200 { "OK" } else { "Internal Server Error" },
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        let base = format!("http://{addr}");
        let root = std::env::temp_dir().join(format!("2xapi-plugbuiltin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let st = mk_state(&root);
        std::fs::write(
            &st.providers_path,
            json!({
                "schema_version": 3, "active_provider_id": "p1",
                "providers": [{ "id": "p1", "name": "t", "agent": "codex", "base_url": base, "api_key": "sk-t", "model": "m1" }]
            })
            .to_string(),
        )
        .unwrap();
        let image = crate::media::store_upload(
            &st.codex_home,
            b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR",
            "image/png",
            "test",
        )
        .unwrap();
        let media_url = format!("/media/{}.{}", image.id, image.ext);

        // 主模型 500 → 切备用成功(备用模型名进入响应)
        let entry = entry_with_models(
            json!([
                {"id":"bad","api":"","note":"主"},
                {"id":"good","api":"","note":"备"}
            ]),
            true,
        );
        let v = body_of(
            builtin_media_failover(
                &st,
                &entry,
                &json!({ "media_url": media_url, "prompt": "什么颜色?" }),
                MediaTool::Describe,
            )
            .await,
        )
        .await;
        assert_eq!(v["ok"], true, "主失败应切备用: {v}");
        assert_eq!(v["data"]["text"], "红色");
        assert_eq!(v["data"]["model"], "good");
        {
            let reqs = seen.lock().unwrap();
            assert!(reqs.iter().any(|r| r.contains("\"bad\"")), "应先试主模型");
            assert!(reqs.iter().any(|r| r.contains("\"good\"")), "再试备用模型");
        }

        // 全败 → 聚合人话
        let entry2 = entry_with_models(
            json!([
                {"id":"bad","api":"","note":"主"},
                {"id":"bad","api":"","note":"备"}
            ]),
            true,
        );
        let v2 = body_of(
            builtin_media_failover(
                &st,
                &entry2,
                &json!({ "media_url": media_url, "prompt": "什么颜色?" }),
                MediaTool::Describe,
            )
            .await,
        )
        .await;
        assert_eq!(v2["ok"], false);
        assert!(
            v2["error"]["message"]
                .as_str()
                .unwrap()
                .contains("主模型 bad"),
            "全败聚合: {v2}"
        );

        // 未配置 models → 原行为(active 供应商默认模型直连,成功)
        let entry3 = entry_with_models(json!([]), true);
        let v3 = body_of(
            builtin_media_failover(
                &st,
                &entry3,
                &json!({ "media_url": media_url, "prompt": "什么颜色?" }),
                MediaTool::Describe,
            )
            .await,
        )
        .await;
        assert_eq!(v3["ok"], true, "未配置 models 走原行为: {v3}");
        assert_eq!(v3["data"]["model"], "m1");
        let _ = std::fs::remove_dir_all(&root);
    }
}
