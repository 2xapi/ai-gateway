//! 平台启动器:统一 launch 入口、codex 路径解析、共享工具(方案 v2 §5)。

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

use std::path::PathBuf;

/// 一次启动的全部参数。
pub(crate) struct LaunchSpec {
    pub temp_dir: PathBuf,
    pub codex_bin: String,
    /// macOS:写入启动脚本(Terminal.app 不继承父 env);Linux/Windows:父进程 env 注入,不落盘。
    pub api_key: String,
    pub model: String,
    pub project_dir: String,
    pub sandbox: String,
    pub extra_args: Vec<String>,
}

/// 统一 codex 参数模板(§5.3):sandbox/extraArgs 来自 API,其余与 M7 一致。
pub(crate) fn codex_args(spec: &LaunchSpec) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-C".into(),
        spec.project_dir.clone(),
        "-m".into(),
        spec.model.clone(),
        "--ephemeral".into(),
        "--skip-git-repo-check".into(),
        "-s".into(),
        spec.sandbox.clone(),
    ];
    args.extend(spec.extra_args.iter().cloned());
    args
}

/// 单引号安全包裹(内部 `'` 转义为 `'\''`),用于生成 shell 脚本。
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 解析 codex CLI 路径:`CODEX_CLI_PATH` → PATH 查找(`command -v` / `where`)→ 平台默认。
pub(crate) fn resolve_codex_bin() -> String {
    if let Ok(p) = std::env::var("CODEX_CLI_PATH") {
        if !p.trim().is_empty() {
            return p.trim().to_string();
        }
    }
    let lookup = if cfg!(windows) {
        std::process::Command::new("cmd")
            .args(["/C", "where codex"])
            .output()
    } else {
        std::process::Command::new("sh")
            .args(["-lc", "command -v codex"])
            .output()
    };
    if let Ok(out) = lookup {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    // 平台默认路径
    let home = std::env::var("HOME").unwrap_or_default();
    let userprofile = std::env::var("USERPROFILE").unwrap_or_default();
    if cfg!(target_os = "macos") {
        "/Applications/ChatGPT.app/Contents/Resources/codex".to_string()
    } else if cfg!(target_os = "windows") {
        format!("{userprofile}\\.codex\\bin\\codex.exe")
    } else {
        format!("{home}/.local/bin/codex")
    }
}

/// 打开系统终端运行 codex(平台实现见子模块;仅此处分平台,其余逻辑全平台共用)。
pub(crate) fn launch(spec: &LaunchSpec) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    return macos::launch(spec);
    #[cfg(target_os = "linux")]
    return linux::launch(spec);
    #[cfg(target_os = "windows")]
    return windows::launch(spec);
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    return Err("当前平台暂不支持".into());
}
