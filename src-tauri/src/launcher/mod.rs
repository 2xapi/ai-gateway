//! Codex 启动器(M8:跨平台核心,macOS + Linux;Windows 骨架待 M8.5)。
//!
//! - 每次「使用」生成独立临时 CODEX_HOME(`/tmp/codex-launch-<uuid>/`,0700),绝不碰 `~/.codex`
//! - config.toml 用 `env_key` 从环境变量读 key;key 不出现在命令行参数(防 ps 泄露)
//! - 平台启动器(platform/)打开系统终端运行交互式 codex CLI(直连中转站;M9 起加网关模式)
//! - 退出清理四条路(方案 v2 §5.4):包装脚本收尾 / lifecycle 后台监控 / stop API / 启动清扫 sweep_orphans
//! - `launcher.json` 标记目录归属(不含 key),清扫只删带标记的目录
//!
//! 与 M7 的差异:模块拆分;0700 加固(G2);sandbox/extraArgs 参数化(解 G6);
//! provider 模式 wire_api 修复(解 G3);**修复 M7 缺陷:`trap EXIT` + `exec` 组合实际不触发清理**
//! (exec 会替换进程映像,EXIT trap 不执行),改为「后台 codex + wait + rm」模式。

mod codex_config;
mod lifecycle;
mod platform;

// main.rs 以 launcher::sweep_orphans()/spawn_monitor() 调用:lifecycle 为私有子模块,此处 re-export
pub use lifecycle::{spawn_monitor, sweep_orphans};

pub(crate) fn resolve_codex_bin() -> String {
    platform::resolve_codex_bin()
}

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use uuid::Uuid;

/// key 注入用的环境变量名(config.toml 的 env_key 指向它)。
pub const ENV_KEY_NAME: &str = "CODEX_LAUNCHER_API_KEY";

/// 目录归属标记文件(内容不含 key);清扫仅认此标记。
pub(crate) const MARKER_FILE: &str = "launcher.json";

/// 临时目录名前缀。
pub(crate) const TEMP_PREFIX: &str = "codex-launch-";

/// 一个启动会话(字段不含 api_key)。
#[derive(Debug, Clone)]
pub struct LaunchSession {
    pub id: String,
    pub temp_dir: PathBuf,
    pub base_url: String,
    pub model: String,
    pub project_dir: String,
    pub sandbox: String,
    pub started_at: String, // 展示用(本地时区)
    pub created_ts: i64,    // 逻辑用(unix 秒)
}

impl LaunchSession {
    /// codex 实际 pid(包装脚本在后台启动 codex 后写入;无文件 = 尚未启动/已清理)。
    pub(crate) fn read_pid(&self) -> Option<u32> {
        std::fs::read_to_string(self.temp_dir.join("codex.pid"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }
}

/// 全局启动器状态(挂在 AppState 上,跨请求共享)。
#[derive(Default)]
pub struct LauncherState {
    pub sessions: Mutex<HashMap<String, LaunchSession>>,
}

/// sandbox 参数解析(默认 workspace-write,与 M7 行为一致)。
pub(crate) fn parse_sandbox(v: Option<&str>) -> Result<String, String> {
    let s = v.unwrap_or("").trim();
    if s.is_empty() {
        return Ok("workspace-write".into());
    }
    match s {
        "read-only" | "workspace-write" | "danger-full-access" => Ok(s.to_string()),
        _ => Err(format!(
            "不支持的 sandbox: {s}(可选 read-only / workspace-write / danger-full-access)"
        )),
    }
}

fn parse_extra_args(input: &Value) -> Vec<String> {
    input
        .get("extraArgs")
        .or_else(|| input.get("extra_args"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// 创建 0700 临时目录(G2 加固:key 所在脚本/配置都在此目录内)。
fn create_temp_dir(id: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("{}{}", TEMP_PREFIX, id));
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("设置临时目录权限失败: {e}"))?;
    }
    Ok(dir)
}

/// 写目录归属标记(不含 key;供启动清扫识别)。
fn write_marker(dir: &Path, created_ts: i64) -> Result<(), String> {
    let marker = json!({ "launcher": "2xapi", "version": 2, "created_at": created_ts });
    std::fs::write(dir.join(MARKER_FILE), marker.to_string())
        .map_err(|e| format!("写 {} 失败: {}", MARKER_FILE, e))
}

/// POST /api/launcher/start
/// body 两种来源:
///   - `{ projectDir, providerId, model? }`:key/base_url/wire_api 从软件 providers.json 取(自己用)
///   - `{ projectDir, baseUrl, apiKey, model, wireApi? }`:手动直连(客户各自 key)
///
/// 通用可选:`sandbox`(默认 workspace-write)、`extraArgs[]`(附加 codex 参数)
pub fn start(state: &LauncherState, input: &Value, providers_path: &Path) -> Result<Value, String> {
    let project_dir = input
        .get("projectDir")
        .or_else(|| input.get("project_dir"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if project_dir.is_empty() {
        return Err("projectDir 不能为空".into());
    }

    // 来源一:软件已存 provider(key 在 providers.json,前端无需接触)
    let provider_id = input
        .get("providerId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let (base_url, api_key, mut model, provider_wire_api) = if !provider_id.is_empty() {
        let data = crate::providers::load(providers_path);
        let p = data
            .providers
            .iter()
            .find(|p| p.id == provider_id)
            .ok_or_else(|| "找不到该 Provider".to_string())?;
        // G3 修复:provider 模式默认用 provider.wire_api(M7 硬编码 responses,chat 中转会连错协议)
        let wire = serde_json::to_value(p.wire_api)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| "responses".to_string());
        (p.base_url.clone(), p.api_key.clone(), p.model.clone(), wire)
    } else {
        // 来源二:手动填写(直连)
        let base_url = input
            .get("baseUrl")
            .or_else(|| input.get("base_url"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let api_key = input
            .get("apiKey")
            .or_else(|| input.get("api_key"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let model = input
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if base_url.is_empty() {
            return Err("baseUrl 不能为空".into());
        }
        if api_key.is_empty() {
            return Err("apiKey 不能为空".into());
        }
        if model.is_empty() {
            return Err("model 不能为空".into());
        }
        (base_url, api_key, model, "responses".to_string())
    };

    // wire_api 优先级:显式传入 > provider.wire_api > responses
    let wire_api = input
        .get("wireApi")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or(provider_wire_api);
    if wire_api != "responses" && wire_api != "chat_completions" {
        return Err(format!("不支持的 wire_api: {wire_api}"));
    }

    // 可选覆盖模型(provider 模式允许覆盖;手动模式此处为原值)
    if let Some(m) = input.get("model").and_then(|v| v.as_str()) {
        let m = m.trim();
        if !m.is_empty() {
            model = m.to_string();
        }
    }
    if model.is_empty() {
        return Err("model 不能为空(provider 未配置默认模型)".into());
    }

    let sandbox = parse_sandbox(input.get("sandbox").and_then(|v| v.as_str()))?;
    let extra_args = parse_extra_args(input);

    let codex = platform::resolve_codex_bin();
    let id = Uuid::new_v4().to_string();
    let temp_dir = create_temp_dir(&id)?;
    let created_ts = chrono::Utc::now().timestamp();

    write_marker(&temp_dir, created_ts)?;
    codex_config::write(&temp_dir, &base_url, &model, &wire_api)?;

    let spec = platform::LaunchSpec {
        temp_dir: temp_dir.clone(),
        codex_bin: codex.clone(),
        api_key,
        model: model.clone(),
        project_dir: project_dir.clone(),
        sandbox: sandbox.clone(),
        extra_args: extra_args.clone(),
    };
    if let Err(e) = platform::launch(&spec) {
        let _ = std::fs::remove_dir_all(&temp_dir); // 启动失败不留目录(含 key 的脚本)
        return Err(e);
    }

    let session = LaunchSession {
        id: id.clone(),
        temp_dir: temp_dir.clone(),
        base_url: base_url.clone(),
        model: model.clone(),
        project_dir: project_dir.clone(),
        sandbox: sandbox.clone(),
        started_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        created_ts,
    };
    state.sessions.lock().unwrap().insert(id.clone(), session);

    Ok(json!({
        "sessionId": id,
        "tempDir": temp_dir.to_string_lossy(),
        "baseUrl": base_url,
        "model": model,
        "projectDir": project_dir,
        "wireApi": wire_api,
        "sandbox": sandbox,
        "codex": codex,
        "note": "已打开系统终端运行 Codex CLI(独立 CODEX_HOME,直连中转站)",
    }))
}

/// POST /api/launcher/stop { sessionId }
pub fn stop(state: &LauncherState, session_id: &str) -> Result<Value, String> {
    let session = {
        let mut m = state.sessions.lock().unwrap();
        m.remove(session_id)
            .ok_or_else(|| "找不到该启动会话".to_string())?
    };

    // 终止 codex 进程(Unix: TERM→KILL;Windows: taskkill /T /F);包装脚本 wait 返回后也会自清
    if let Some(pid) = session.read_pid() {
        lifecycle::terminate_tree(pid);
    }

    // 清理临时目录(脚本收尾为主;这里强制兜底一次)
    let _ = std::fs::remove_dir_all(&session.temp_dir);

    Ok(json!({
        "sessionId": session_id,
        "cleaned": true,
        "tempDir": session.temp_dir.to_string_lossy(),
    }))
}

/// GET /api/launcher/status
pub fn status(state: &LauncherState) -> Value {
    let m = state.sessions.lock().unwrap();
    let sessions: Vec<Value> = m
        .values()
        .map(|s| {
            let pid = s.read_pid();
            json!({
                "sessionId": s.id,
                "tempDir": s.temp_dir.to_string_lossy(),
                "baseUrl": s.base_url,
                "model": s.model,
                "projectDir": s.project_dir,
                "sandbox": s.sandbox,
                "startedAt": s.started_at,
                "pid": pid,
                "alive": pid.map(lifecycle::is_alive).unwrap_or(false),
            })
        })
        .collect();
    json!({ "sessions": sessions })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_default_and_valid_values() {
        assert_eq!(parse_sandbox(None).unwrap(), "workspace-write");
        assert_eq!(parse_sandbox(Some("")).unwrap(), "workspace-write");
        assert_eq!(parse_sandbox(Some("read-only")).unwrap(), "read-only");
        assert_eq!(
            parse_sandbox(Some(" danger-full-access ")).unwrap(),
            "danger-full-access"
        );
    }

    #[test]
    fn sandbox_rejects_unknown() {
        assert!(parse_sandbox(Some("yolo")).is_err());
        assert!(parse_sandbox(Some("--danger")).is_err());
    }

    #[test]
    fn extra_args_parse_and_filter() {
        let v: Value =
            serde_json::from_str(r#"{"extraArgs":["--verbose","  ","-c","x=1",42]}"#).unwrap();
        assert_eq!(parse_extra_args(&v), vec!["--verbose", "-c", "x=1"]);
        assert!(parse_extra_args(&serde_json::json!({})).is_empty());
    }
}
