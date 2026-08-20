//! 开机自启(launchd,macOS):写/删 `~/Library/LaunchAgents/com.2xapi.codexconsole.plist`。
//! 核心函数全部显式传目录,便于 tempdir 单测;handlers 以 launch_agents_dir() 兜真实路径。

const PLIST_NAME: &str = "com.2xapi.codexconsole.plist";
const LABEL: &str = "com.2xapi.codexconsole";
const APP_PATH: &str = "/Applications/2xapi Codex Console.app/Contents/MacOS/console-2xapi";

pub fn supported() -> bool {
    cfg!(target_os = "macos")
}

fn executable_path() -> std::path::PathBuf {
    std::env::var_os("CODEX_CONSOLE_EXECUTABLE")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| std::path::PathBuf::from(APP_PATH))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// launchd LaunchAgents 目录(Home 回退逻辑与 codex_home 一致:Windows 无 HOME 时用 USERPROFILE)。
pub fn launch_agents_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("USERPROFILE").ok())
        .unwrap_or_default();
    std::path::PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
}

pub fn plist_path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join(PLIST_NAME)
}

/// 读:文件存在即视为已开启(launchd 侧不额外解析)。
pub fn enabled(dir: &std::path::Path) -> bool {
    plist_path(dir).exists()
}

/// 写:创建 plist(RunAtLoad 随登录加载);删:移除文件。幂等:重复开/关不报错。
pub fn set(dir: &std::path::Path, enable: bool) -> Result<(), String> {
    if !supported() {
        return Err("当前平台不支持 macOS launchd 自启".into());
    }
    let path = plist_path(dir);
    if enable {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建 LaunchAgents 目录失败: {e}"))?;
        std::fs::write(&path, plist_xml(&executable_path()))
            .map_err(|e| format!("写 plist 失败: {e}"))
    } else {
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("删 plist 失败: {e}")),
        }
    }
}

fn plist_xml(program: &std::path::Path) -> String {
    let program = xml_escape(&program.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{program}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "2xapi-autostart-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn write_then_delete_roundtrip() {
        let d = tmp_dir("roundtrip");
        assert!(!enabled(&d));
        set(&d, true).unwrap();
        assert!(enabled(&d));
        // plist 内容含 Label 与 RunAtLoad
        let raw = std::fs::read_to_string(plist_path(&d)).unwrap();
        assert!(raw.contains(LABEL));
        assert!(raw.contains("<key>RunAtLoad</key>"));
        assert!(raw.contains(&xml_escape(&executable_path().to_string_lossy())));
        set(&d, false).unwrap();
        assert!(!enabled(&d));
        assert!(!plist_path(&d).exists());
    }

    #[test]
    fn idempotent_enable_and_disable() {
        let d = tmp_dir("idem");
        set(&d, true).unwrap();
        set(&d, true).unwrap(); // 重复开不报错
        assert!(enabled(&d));
        set(&d, false).unwrap();
        set(&d, false).unwrap(); // 重复关(文件已不在)不报错
        assert!(!enabled(&d));
    }
}
