//! macOS 启动器:生成 start.command(env 注入 + 后台 codex + wait + 收尾清理)→ `open -a Terminal`。
//!
//! Terminal.app 由 launchd 拉起、不继承本进程 env,故 key 只能经脚本注入:
//! 存活期 = codex 会话期,目录 0700、随机名、退出即删、启动清扫兜底(M11 内嵌终端可彻底零落盘)。
//!
//! 注意:M7 的 `trap 'rm' EXIT` + `exec codex` 组合实际不生效——exec 替换进程映像,
//! EXIT trap 不会执行(codex 退出后目录不会被清)。M8 改为「后台启动 codex → 记 pid →
//! wait → rm」:正常退出由脚本收尾,异常路径由 stop API / 后台监控 / 启动清扫兜底。

use super::{codex_args, sh_quote, LaunchSpec};
use crate::launcher::ENV_KEY_NAME;
use std::os::unix::fs::PermissionsExt;

pub(crate) fn launch(spec: &LaunchSpec) -> Result<(), String> {
    let home = spec.temp_dir.to_string_lossy().to_string();
    let script_path = spec.temp_dir.join("start.command");
    let pid_file = spec.temp_dir.join("codex.pid");
    let quoted_args = codex_args(spec)
        .iter()
        .map(|a| sh_quote(a))
        .collect::<Vec<_>>()
        .join(" ");

    let script = format!(
        "#!/bin/bash\n\
         export CODEX_HOME={home_q}\n\
         export {env}={key_q}\n\
         cd {proj_q} 2>/dev/null || true\n\
         {codex_q} {args} &\n\
         echo $! > {pid_q}\n\
         wait $!\n\
         rm -rf -- {home_q}\n",
        home_q = sh_quote(&home),
        env = ENV_KEY_NAME,
        key_q = sh_quote(&spec.api_key),
        proj_q = sh_quote(&spec.project_dir),
        codex_q = sh_quote(&spec.codex_bin),
        args = quoted_args,
        pid_q = sh_quote(&pid_file.to_string_lossy()),
    );
    std::fs::write(&script_path, script).map_err(|e| format!("写启动脚本失败: {e}"))?;
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("设置脚本权限失败: {e}"))?;

    // .command 由 Terminal 打开执行;Terminal 不继承父 env → env 全部经脚本注入
    std::process::Command::new("open")
        .args(["-a", "Terminal"])
        .arg(&script_path)
        .spawn()
        .map_err(|e| format!("打开终端失败: {e}"))?;
    Ok(())
}
