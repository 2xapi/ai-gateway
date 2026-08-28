//! 生命周期:跨平台进程管理 + 后台退出监控 + 启动清扫(方案 v2 §5.3 / §5.4)。
//!
//! 四条退出路径全覆盖:
//! 1. codex 正常退出 → 包装脚本收尾(wait 后 rm)
//! 2. 用户直接关终端窗 → SIGHUP 杀 codex → 监控线程检测 pid 死亡 → 清理
//! 3. UI 点「停止」→ stop API(terminate_tree + 删目录)
//! 4. app 崩溃被杀 → 下次启动 sweep_orphans 按 launcher.json 标记清扫

use crate::launcher::{LaunchSession, LauncherState, MARKER_FILE, TEMP_PREFIX};
use serde_json::Value;
use std::sync::Arc;

/// 进程是否存活(Unix: kill -0;Windows: tasklist 查询)。
pub(crate) fn is_alive(pid: u32) -> bool {
    if cfg!(windows) {
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                String::from_utf8_lossy(&o.stdout).contains(&pid.to_string())
            }
            _ => false,
        }
    } else {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

/// 终止进程树(Unix: TERM→800ms→KILL;Windows: taskkill /T /F 连控制台整树)。
pub(crate) fn terminate_tree(pid: u32) {
    if cfg!(windows) {
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID"])
            .arg(pid.to_string())
            .status();
    } else {
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
        std::thread::sleep(std::time::Duration::from_millis(800));
        if is_alive(pid) {
            let _ = std::process::Command::new("kill")
                .arg("-KILL")
                .arg(pid.to_string())
                .status();
        }
    }
}

/// 启动后台监控线程:每 2s 检查各会话 codex pid;死亡即清理目录并移除会话。
/// 同时兜底「终端打开失败」:pid 文件 120s 未出现 → 回收目录。
pub fn spawn_monitor(state: Arc<LauncherState>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(2));
        let now = chrono::Utc::now().timestamp();
        let snapshot: Vec<LaunchSession> = {
            let m = state.sessions.lock().unwrap();
            m.values().cloned().collect()
        };
        for s in snapshot {
            let dead = match s.read_pid() {
                Some(pid) => !is_alive(pid),
                // pid 文件未出现:可能终端没开成功,120s 宽限后回收
                None => now - s.created_ts > 120,
            };
            if dead {
                let _ = std::fs::remove_dir_all(&s.temp_dir);
                state.sessions.lock().unwrap().remove(&s.id);
            }
        }
    });
}

/// 清扫动作判定(纯函数,便于单测)。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SweepAction {
    Keep,
    Delete,
}

/// - has_marker:目录是否带本工具标记(不带 = 非本工具目录,绝不碰)
/// - pid_alive:Some(存活/已死);None = pid 文件不存在(尚未启动或启动失败)
/// - age_secs:目录年龄
pub(crate) fn sweep_action(
    has_marker: bool,
    pid_alive: Option<bool>,
    age_secs: i64,
) -> SweepAction {
    if !has_marker {
        return SweepAction::Keep;
    }
    if age_secs > 48 * 3600 {
        return SweepAction::Delete; // 超期一律回收
    }
    match pid_alive {
        Some(true) => SweepAction::Keep, // codex 还在跑(app 重启后发现的活跃会话)
        Some(false) => SweepAction::Delete, // 进程已死 → 残留
        None if age_secs > 300 => SweepAction::Delete, // 无 pid 且非新建 → 启动失败残留
        None => SweepAction::Keep,       // 刚创建,pid 未就绪
    }
}

/// 启动清扫:扫描临时目录根下 `codex-launch-*`,按「标记 + 存活 + 年龄」决定去留。
/// 只删带 launcher.json 标记(launcher=="2xapi")的目录,防误删他人文件。
pub fn sweep_orphans() {
    let root = std::env::temp_dir();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    let now = chrono::Utc::now().timestamp();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with(TEMP_PREFIX) || !e.path().is_dir() {
            continue;
        }
        let dir = e.path();
        let marker: Option<Value> = std::fs::read_to_string(dir.join(MARKER_FILE))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());
        let is_ours = marker
            .as_ref()
            .and_then(|m| m.get("launcher"))
            .and_then(|v| v.as_str())
            == Some("2xapi");
        if !is_ours {
            continue;
        }
        // 创建时间:标记里的 created_at,缺失退回目录 mtime
        let created = marker
            .as_ref()
            .and_then(|m| m.get("created_at"))
            .and_then(|v| v.as_i64())
            .or_else(|| {
                e.metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| {
                        t.duration_since(std::time::UNIX_EPOCH)
                            .ok()
                            .map(|d| d.as_secs() as i64)
                    })
            })
            .unwrap_or(0);
        let pid_alive = std::fs::read_to_string(dir.join("codex.pid"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .map(is_alive);
        if sweep_action(true, pid_alive, now - created) == SweepAction::Delete {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_keeps_foreign_dirs() {
        // 无标记 → 无论状态如何都不动
        assert_eq!(sweep_action(false, Some(false), 999_999), SweepAction::Keep);
        assert_eq!(sweep_action(false, None, 999_999), SweepAction::Keep);
    }

    #[test]
    fn sweep_deletes_dead_and_stale() {
        // 进程已死 → 删
        assert_eq!(sweep_action(true, Some(false), 60), SweepAction::Delete);
        // 超过 48h → 删(即使 pid 显示存活,大概率 pid 复用)
        assert_eq!(
            sweep_action(true, Some(true), 48 * 3600 + 1),
            SweepAction::Delete
        );
        // 无 pid 且非新建(>300s)→ 删(启动失败残留)
        assert_eq!(sweep_action(true, None, 301), SweepAction::Delete);
    }

    #[test]
    fn sweep_keeps_alive_and_fresh() {
        // codex 在跑 → 保留(app 重启场景)
        assert_eq!(sweep_action(true, Some(true), 3600), SweepAction::Keep);
        // 刚创建、pid 未就绪 → 保留(等待脚本写入)
        assert_eq!(sweep_action(true, None, 10), SweepAction::Keep);
    }
}
