//! Linux 启动器:探测终端模拟器 → 直子进程注入 env(key 零落盘)→ 运行不含 key 的包装脚本。
//!
//! - key/`CODEX_HOME` 经 `Command::env` 注入:终端 → bash → codex 全链路继承,**不写任何文件**
//! - 包装脚本(0700)仅含路径与参数,负责:后台启动 codex → 记 pid → wait → 收尾 rm
//! - 终端优先「新进程型」(kitty/alacritty/xterm…),env 必定继承;
//!   gnome-terminal/konsole 等单实例型经既有服务进程开窗,env 转发依赖其实现(现代版本可用)
//! - 显式指定:设 `TERMINAL`(如 `kitty -e`、`gnome-terminal --`),按 `sh -c "$TERMINAL bash <脚本>"` 执行

use super::{codex_args, sh_quote, LaunchSpec};
use crate::launcher::ENV_KEY_NAME;
use std::os::unix::fs::PermissionsExt;

/// 候选终端(名字, 参数前缀):按序探测,优先非单实例型(§5.1)。
const CANDIDATES: &[(&str, &str)] = &[
    ("kitty", "-e"),
    ("alacritty", "-e"),
    ("wezterm", "start --"),
    ("xfce4-terminal", "-x"),
    ("konsole", "-e"),
    ("gnome-terminal", "--"),
    ("x-terminal-emulator", "-e"),
    ("xterm", "-e"),
];

fn which(bin: &str) -> Option<String> {
    let out = std::process::Command::new("sh")
        .args(["-lc", &format!("command -v {bin}")])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

pub(crate) fn launch(spec: &LaunchSpec) -> Result<(), String> {
    let home = spec.temp_dir.to_string_lossy().to_string();
    let script_path = spec.temp_dir.join("start.sh");
    let pid_file = spec.temp_dir.join("codex.pid");
    let quoted_args = codex_args(spec)
        .iter()
        .map(|a| sh_quote(a))
        .collect::<Vec<_>>()
        .join(" ");

    // 包装脚本:不含 key(key 经 env 继承);后台 codex + 记 pid + wait + 收尾清理
    let script = format!(
        "#!/bin/bash\n\
         export CODEX_HOME={home_q}\n\
         cd {proj_q} 2>/dev/null || true\n\
         {codex_q} {args} &\n\
         echo $! > {pid_q}\n\
         wait $!\n\
         rm -rf -- {home_q}\n",
        home_q = sh_quote(&home),
        proj_q = sh_quote(&spec.project_dir),
        codex_q = sh_quote(&spec.codex_bin),
        args = quoted_args,
        pid_q = sh_quote(&pid_file.to_string_lossy()),
    );
    std::fs::write(&script_path, script).map_err(|e| format!("写包装脚本失败: {e}"))?;
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("设置脚本权限失败: {e}"))?;

    // 直子进程启动终端 → env 完整继承(key 不落盘、不进 argv)
    let script_str = script_path.to_string_lossy().to_string();
    let spawn_result = if let Ok(t) = std::env::var("TERMINAL") {
        let t = t.trim().to_string();
        if t.is_empty() {
            return Err("$TERMINAL 为空".into());
        }
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{t} bash {}", sh_quote(&script_str)))
            .env("CODEX_HOME", &home)
            .env(ENV_KEY_NAME, &spec.api_key)
            .spawn()
    } else {
        let (bin, prefix) = CANDIDATES
            .iter()
            .find_map(|(name, prefix)| which(name).map(|b| (b, prefix)))
            .ok_or_else(|| {
                "未找到终端模拟器:可设 $TERMINAL(如 \"kitty -e\"),或安装 kitty/xterm".to_string()
            })?;
        let mut cmd = std::process::Command::new(bin);
        // 前缀可能含多个参数(如 wezterm 的 "start --")
        for a in prefix.split_whitespace() {
            cmd.arg(a);
        }
        cmd.arg("bash")
            .arg(&script_str)
            .env("CODEX_HOME", &home)
            .env(ENV_KEY_NAME, &spec.api_key)
            .spawn()
    };
    spawn_result.map_err(|e| format!("启动终端失败: {e}"))?;
    Ok(())
}
