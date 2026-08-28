//! Codex 官方认证边界。
//!
//! 2xapi 只调用官方 CLI 的 `login status/login/logout`，不解析或改写
//! `auth.json`，也不直接访问操作系统凭据存储。该模块不返回 CLI 原始输出，
//! 避免 access/refresh token 进入 API、日志或前端状态。

use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(8);
const CACHE_TTL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoginState {
    SignedIn,
    SignedOut,
    Unknown,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LoginStatus {
    pub state: LoginState,
    pub method: String,
    pub source: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoginProcess {
    pub started: bool,
    pub pid: Option<u32>,
    pub cli: String,
}

static LOGIN_CACHE: OnceLock<Mutex<Option<(PathBuf, Instant, LoginStatus)>>> = OnceLock::new();

pub fn invalidate_login_cache() {
    if let Some(cache) = LOGIN_CACHE.get() {
        if let Ok(mut guard) = cache.lock() {
            *guard = None;
        }
    }
}

fn cli_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(path) = std::env::var("CODEX_CLI_PATH") {
        if !path.trim().is_empty() {
            candidates.push(PathBuf::from(path));
        }
    }
    if cfg!(target_os = "macos") {
        candidates.push(PathBuf::from(
            "/Applications/ChatGPT.app/Contents/Resources/codex",
        ));
    }
    if cfg!(target_os = "windows") {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            candidates.push(PathBuf::from(&local).join("Programs/codex/codex.exe"));
            candidates.push(PathBuf::from(local).join("Programs/Codex/codex.exe"));
        }
        if let Ok(program) = std::env::var("ProgramFiles") {
            candidates.push(PathBuf::from(program).join("Codex/codex.exe"));
        }
        candidates.push(PathBuf::from("codex.exe"));
    } else {
        candidates.push(PathBuf::from("codex"));
    }
    candidates
}

pub fn resolve_codex_cli() -> Option<PathBuf> {
    for candidate in cli_candidates() {
        if candidate.components().count() == 1 {
            // 不要把 CODEX_CLI_PATH / 候选名拼进 shell。该值来自进程环境，
            // 通过 `sh -lc "command -v ..."` 会把分号、反引号等解释成命令。
            // 直接按 PATH 查找既跨平台，也不会产生 shell 注入。
            if let Some(path) = lookup_path(&candidate) {
                return Some(path);
            }
        } else if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn lookup_path(name: &Path) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path_var) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn cli_command(cli: &Path, args: &[&str]) -> Command {
    #[cfg(windows)]
    {
        let is_script = cli
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| {
                value.eq_ignore_ascii_case("cmd") || value.eq_ignore_ascii_case("bat")
            });
        if is_script {
            let mut command = Command::new("cmd.exe");
            command.arg("/C").arg(cli).args(args);
            return command;
        }
    }
    let mut command = Command::new(cli);
    command.args(args);
    command
}

fn spawn_cli(cli: &Path, args: &[&str], codex_home: &Path) -> Result<Child, String> {
    cli_command(cli, args)
        .env("CODEX_HOME", codex_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动 Codex CLI 失败: {e}"))
}

fn wait_output(mut child: Child, timeout: Duration) -> Result<Output, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child
                    .wait_with_output()
                    .map_err(|e| format!("读取 Codex CLI 结果失败: {e}"))
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Codex CLI 操作超时".into());
            }
            Ok(None) => thread::sleep(Duration::from_millis(40)),
            Err(e) => return Err(format!("等待 Codex CLI 失败: {e}")),
        }
    }
}

fn classify_output(output: &Output) -> LoginStatus {
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let lower = text.to_lowercase();
    let method = if lower.contains("chatgpt") || lower.contains("openai account") {
        "chatgpt"
    } else if lower.contains("api key") || lower.contains("api_key") || lower.contains("apikey") {
        "api_key"
    } else {
        "unknown"
    };
    let signed_out = [
        "not logged",
        "logged out",
        "not authenticated",
        "no authentication",
        "未登录",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    let signed_in = [
        "logged in",
        "authenticated",
        "signed in",
        "已登录",
        "已认证",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    if output.status.success() && signed_in && !signed_out {
        LoginStatus {
            state: LoginState::SignedIn,
            method: method.into(),
            source: "codex_cli",
            message: if method == "chatgpt" {
                "Codex 已使用 ChatGPT 登录".into()
            } else {
                "Codex 已认证".into()
            },
        }
    } else if signed_out {
        LoginStatus {
            state: LoginState::SignedOut,
            method: method.into(),
            source: "codex_cli",
            message: "Codex 当前未完成可用登录".into(),
        }
    } else {
        LoginStatus {
            state: LoginState::Unknown,
            method: method.into(),
            source: "codex_cli",
            message: "无法识别 Codex 登录状态".into(),
        }
    }
}

pub fn probe_login(codex_home: &Path) -> LoginStatus {
    // 单元/E2E 测试不得读取开发机真实 Codex 登录态；需要覆盖 CLI 解析时，
    // 测试显式设置 CODEX_CLI_PATH 指向替身。
    #[cfg(test)]
    if std::env::var_os("CODEX_CLI_PATH").is_none() {
        return LoginStatus {
            state: LoginState::Unknown,
            method: "unknown".into(),
            source: "codex_cli",
            message: "测试环境未配置 Codex CLI 替身".into(),
        };
    }
    let Some(cli) = resolve_codex_cli() else {
        return LoginStatus {
            state: LoginState::Unknown,
            method: "unknown".into(),
            source: "codex_cli",
            message: "未找到 Codex CLI".into(),
        };
    };
    let child = match spawn_cli(&cli, &["login", "status"], codex_home) {
        Ok(child) => child,
        Err(message) => {
            return LoginStatus {
                state: LoginState::Unknown,
                method: "unknown".into(),
                source: "codex_cli",
                message,
            }
        }
    };
    match wait_output(child, DEFAULT_TIMEOUT) {
        Ok(output) => classify_output(&output),
        Err(message) => LoginStatus {
            state: LoginState::Unknown,
            method: "unknown".into(),
            source: "codex_cli",
            message,
        },
    }
}

pub fn probe_login_cached(codex_home: &Path) -> LoginStatus {
    let cache = LOGIN_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some((path, at, status)) = guard.as_ref() {
            if path == codex_home && at.elapsed() < CACHE_TTL {
                return status.clone();
            }
        }
    }
    let status = probe_login(codex_home);
    if let Ok(mut guard) = cache.lock() {
        *guard = Some((codex_home.to_path_buf(), Instant::now(), status.clone()));
    }
    status
}

pub fn start_login(codex_home: &Path, device_auth: bool) -> Result<LoginProcess, String> {
    let cli =
        resolve_codex_cli().ok_or_else(|| "未找到 Codex CLI，请先安装官方 Codex".to_string())?;
    let mut args = vec!["login"];
    if device_auth {
        args.push("--device-auth");
    }
    let child = cli_command(&cli, &args)
        .env("CODEX_HOME", codex_home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("启动官方登录失败: {e}"))?;
    Ok(LoginProcess {
        started: true,
        pid: Some(child.id()),
        cli: cli.to_string_lossy().into_owned(),
    })
}

pub fn logout(codex_home: &Path) -> Result<LoginStatus, String> {
    let cli =
        resolve_codex_cli().ok_or_else(|| "未找到 Codex CLI，无法执行官方 logout".to_string())?;
    let child = spawn_cli(&cli, &["logout"], codex_home)?;
    let output = wait_output(child, DEFAULT_TIMEOUT)?;
    if !output.status.success() {
        return Err("官方 codex logout 未成功完成".into());
    }
    invalidate_login_cache();
    Ok(LoginStatus {
        state: LoginState::SignedOut,
        method: "unknown".into(),
        source: "codex_cli",
        message: "官方 logout 已完成".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;

    #[cfg(unix)]
    fn status(code: i32) -> std::process::ExitStatus {
        std::process::ExitStatus::from_raw(code)
    }
    #[cfg(windows)]
    fn status(code: i32) -> std::process::ExitStatus {
        std::process::ExitStatus::from_raw(code as u32)
    }

    fn output(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: status(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn classifies_chatgpt_login_without_exposing_output() {
        let status = classify_output(&output(
            0,
            "Logged in using ChatGPT",
            "refresh_token=secret",
        ));
        assert_eq!(status.state, LoginState::SignedIn);
        assert_eq!(status.method, "chatgpt");
        assert!(!status.message.contains("secret"));
    }

    #[test]
    fn classifies_api_key_and_signed_out() {
        assert_eq!(
            classify_output(&output(0, "Authenticated with API key", "")).method,
            "api_key"
        );
        assert_eq!(
            classify_output(&output(1, "Not logged in", "")).state,
            LoginState::SignedOut
        );
    }

    #[test]
    fn unknown_cli_output_is_not_treated_as_signed_in_or_signed_out() {
        assert_eq!(
            classify_output(&output(0, "status unavailable", "")).state,
            LoginState::Unknown
        );
        assert_eq!(
            classify_output(&output(1, "network unavailable", "")).state,
            LoginState::Unknown
        );
    }
}
