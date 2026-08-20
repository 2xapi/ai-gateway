//! Windows 启动器(骨架,待 M8.5 联调;方案 v2 §5.1)。
//!
//! 设计:
//! - cmd.exe 直子进程 + CREATE_NEW_CONSOLE:新控制台进程,父进程 env 完整继承 → **key 零落盘**
//! - 运行不含 key 的包装 .bat:前台跑 codex → 退出后 `rmdir /s /q` 清理临时目录
//! - 不用 Windows Terminal(wt):常单实例运行,新标签挂既有进程,父 env 不继承会丢 key
//! - M8.5 待办:pid 捕获(批处理取不到子进程 pid,需改 PowerShell `Start-Process -PassThru`
//!   写 codex.pid,stop/监控才能用);bat 参数转义对复杂参数的兼容验证;实机冒烟
//!
//! 注意:本文件仅在 Windows 目标编译;未实机验证前不建议发布给 Windows 用户(M8.5)。

use super::{codex_args, LaunchSpec};

pub(crate) fn launch(spec: &LaunchSpec) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

    let home = spec.temp_dir.to_string_lossy().to_string();
    let bat_path = spec.temp_dir.join("start.bat");

    // 参数双引号包裹(批处理转义能力有限,复杂参数的兼容性待 M8.5 用 PowerShell 包装验证)
    let quoted = codex_args(spec)
        .iter()
        .map(|a| format!("\"{}\"", a.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" ");

    let bat = format!(
        "@echo off\r\n\
         cd /d \"{proj}\"\r\n\
         \"{codex}\" {args}\r\n\
         rmdir /s /q \"{home}\"\r\n",
        proj = spec.project_dir.replace('"', ""),
        codex = spec.codex_bin.replace('"', ""),
        args = quoted,
        home = home.replace('"', ""),
    );
    std::fs::write(&bat_path, bat).map_err(|e| format!("写启动脚本失败: {e}"))?;

    // cmd /k:codex 退出后窗口保留(rmdir 已执行),供用户看输出;强关窗场景由启动清扫兜底
    std::process::Command::new("cmd.exe")
        .arg("/k")
        .arg(&bat_path)
        .env("CODEX_HOME", &home)
        .env(crate::launcher::ENV_KEY_NAME, &spec.api_key)
        .creation_flags(CREATE_NEW_CONSOLE)
        .spawn()
        .map_err(|e| format!("打开控制台失败: {e}"))?;
    Ok(())
}
