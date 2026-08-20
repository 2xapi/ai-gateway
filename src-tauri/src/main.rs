// Windows GUI 程序不弹控制台窗口(release 生效,debug 保留控制台便于排障)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod server;
// M8:launcher 模块根为 launcher/mod.rs。显式 #[path] 是为了兼容旧 launcher.rs
// 还未删除的过渡期(rustc 见到两者并存会报 ambiguous);删除旧文件后此写法同样有效。
mod acclines;
mod agents;
mod auth;
mod autostart;
mod backups;
mod claude_sessions;
mod config;
mod desktop;
mod diagnose;
mod gateway;
mod gateway_conv;
mod gateway_gemini_conv;
mod grok_config;
mod history;
mod keypool;
#[path = "launcher/mod.rs"]
mod launcher;
mod media;
mod media_tools;
mod nodecreds;
mod plugins;
mod probe;
mod providers;
mod registry;
mod sessions;
mod updater;
mod usage_stats;

use std::net::TcpListener;
use std::sync::atomic::Ordering;
use tauri::menu::{IsMenuItem, Menu, MenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, WebviewWindowBuilder};

fn codex_home() -> std::path::PathBuf {
    // Windows 无 HOME 环境变量 → 回退 USERPROFILE,否则 home 解析为空导致 .codex 写错位置
    let home = std::env::var("HOME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("USERPROFILE").ok())
        .unwrap_or_default();
    let h = std::env::var("CODEX_HOME").unwrap_or_else(|_| format!("{}/.codex", home));
    std::path::PathBuf::from(h)
}

// ── 托盘菜单(竞品吸收 1.1-2):纯逻辑抽函数便于单测 ────────────

/// 托盘图标句柄(menu 重建需要 &self;AppState 已 manage,providers_path/网关开关都从那里取)。
struct TrayState {
    icon: std::sync::Mutex<tauri::tray::TrayIcon>,
}

/// 菜单项规格(与 tauri 解耦,单测零窗口)。
#[derive(Debug, Clone, PartialEq)]
enum TraySpecKind {
    Item,
    /// 子菜单:(provider_id, 显示名) 列表
    Submenu(Vec<(String, String)>),
}

#[derive(Debug, Clone, PartialEq)]
struct TraySpec {
    id: String,
    label: String,
    kind: TraySpecKind,
}

/// 托盘菜单结构:当前供应商(active 名,点击切到下一个)/ 切换供应商(子菜单,点击 set_active)
/// / 网关开/关(toggle 内存态)/ 打开主界面 / 退出。
fn tray_items(
    providers: &[crate::providers::Provider],
    active: Option<&str>,
    gate: bool,
) -> Vec<TraySpec> {
    let active_label = providers
        .iter()
        .find(|p| Some(p.id.as_str()) == active)
        .map(|p| p.name.clone())
        .unwrap_or_else(|| "未设置".to_string());
    vec![
        TraySpec {
            id: "active".into(),
            label: format!("当前供应商:{active_label}"),
            kind: TraySpecKind::Item,
        },
        TraySpec {
            id: "providers".into(),
            label: "切换供应商".into(),
            kind: TraySpecKind::Submenu(
                providers
                    .iter()
                    .map(|p| (p.id.clone(), p.name.clone()))
                    .collect(),
            ),
        },
        TraySpec {
            id: "gate".into(),
            label: if gate { "网关:开" } else { "网关:关" }.into(),
            kind: TraySpecKind::Item,
        },
        TraySpec {
            id: "show".into(),
            label: "打开主界面".into(),
            kind: TraySpecKind::Item,
        },
        TraySpec {
            id: "quit".into(),
            label: "退出(关闭网关)".into(),
            kind: TraySpecKind::Item,
        },
    ]
}

/// 「当前供应商」点击切换:active 的下一个(末位回绕);无 active/active 不在列表 → 首个;空列表 → None。
fn next_provider_id(
    providers: &[crate::providers::Provider],
    active: Option<&str>,
) -> Option<String> {
    if providers.is_empty() {
        return None;
    }
    let idx = providers.iter().position(|p| Some(p.id.as_str()) == active);
    let next = match idx {
        Some(i) => (i + 1) % providers.len(),
        None => 0,
    };
    Some(providers[next].id.clone())
}

fn build_tray_menu(
    app: &AppHandle,
    providers: &[crate::providers::Provider],
    active: Option<&str>,
    gate: bool,
) -> tauri::Result<Menu<tauri::Wry>> {
    let mut refs: Vec<Box<dyn IsMenuItem<tauri::Wry>>> = Vec::new();
    for spec in tray_items(providers, active, gate) {
        match spec.kind {
            TraySpecKind::Item => refs.push(Box::new(MenuItem::with_id(
                app,
                &spec.id,
                &spec.label,
                true,
                None::<&str>,
            )?)),
            TraySpecKind::Submenu(entries) => {
                let sub_items: Vec<MenuItem<tauri::Wry>> = entries
                    .iter()
                    .map(|(id, name)| {
                        MenuItem::with_id(app, format!("provider:{id}"), name, true, None::<&str>)
                    })
                    .collect::<Result<_, _>>()?;
                let sub_refs: Vec<&dyn IsMenuItem<tauri::Wry>> = sub_items
                    .iter()
                    .map(|i| i as &dyn IsMenuItem<tauri::Wry>)
                    .collect();
                refs.push(Box::new(Submenu::with_items(
                    app,
                    &spec.label,
                    true,
                    &sub_refs,
                )?));
            }
        }
    }
    let menu_refs: Vec<&dyn IsMenuItem<tauri::Wry>> = refs.iter().map(|b| b.as_ref()).collect();
    Menu::with_items(app, &menu_refs)
}

/// 按 providers.json 当前状态重建托盘菜单(切换供应商 / 网关开关后调用)。
fn rebuild_tray(app: &AppHandle) {
    let st = app.state::<server::AppState>();
    let ts = app.state::<TrayState>();
    let data = crate::providers::load(&st.providers_path);
    match build_tray_menu(
        app,
        &data.providers,
        data.active_provider_id.as_deref(),
        st.tray_gate_enabled.load(Ordering::Relaxed),
    ) {
        Ok(menu) => {
            if let Err(e) = ts.icon.lock().unwrap().set_menu(Some(menu)) {
                eprintln!("[tray] 重建菜单失败: {e}");
            }
        }
        Err(e) => eprintln!("[tray] 重建菜单失败: {e}"),
    }
}

fn show_main_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

// ── deeplink 一键导入(竞品吸收 1.1-5):2xapi://import?name=&baseUrl=&apiKey=&model=&wire= ──

#[derive(Debug, Clone, PartialEq)]
struct ImportParams {
    name: String,
    base_url: String,
    api_key: String,
    model: Option<String>,
    wire: Option<String>,
}

/// 解析 + 契约校验(baseUrl 须 http(s)://、name/apiKey 非空)。纯函数,可单测。
///
/// 手动解析而非 url crate:`2xapi` 以数字开头,不是 RFC 3986 合法 scheme,url crate 直接拒绝;
/// macOS LaunchServices 按 ":" 前缀分发(宽容接受),此处按同一规则解析。
fn parse_import_url(url: &str) -> Result<ImportParams, String> {
    let (scheme, rest) = url.split_once(':').ok_or("链接无效(缺少 ://)")?;
    if scheme != "2xapi" {
        return Err(format!("非 2xapi:// 协议: {scheme}"));
    }
    let rest = rest.strip_prefix("//").unwrap_or(rest); // 兼容 2xapi:// 与 2xapi:
    let (host, query) = rest
        .split_once('?')
        .map(|(h, q)| (h, Some(q)))
        .unwrap_or((rest, None));
    if host != "import" {
        return Err("未知 2xapi 操作(仅支持 import)".into());
    }
    let (mut name, mut base_url, mut api_key, mut model, mut wire) = (None, None, None, None, None);
    if let Some(q) = query {
        for pair in q.split('&') {
            let Some((k, v)) = pair.split_once('=') else {
                continue;
            };
            let v = percent_decode(v);
            match k {
                "name" => name = Some(v),
                "baseUrl" | "base_url" => base_url = Some(v),
                "apiKey" | "api_key" => api_key = Some(v),
                "model" => model = Some(v),
                "wire" => wire = Some(v),
                _ => {}
            }
        }
    }
    let name = name
        .filter(|s| !s.trim().is_empty())
        .ok_or("缺少 name 参数")?;
    let base_url = base_url
        .filter(|s| s.starts_with("http://") || s.starts_with("https://"))
        .ok_or("baseUrl 缺失或非 http(s):// 开头")?;
    let api_key = api_key
        .filter(|s| !s.trim().is_empty())
        .ok_or("缺少 apiKey 参数")?;
    Ok(ImportParams {
        name,
        base_url,
        api_key,
        model: model.filter(|s| !s.trim().is_empty()),
        wire: wire.filter(|s| !s.trim().is_empty()),
    })
}

/// 极简 percent 解码(%XX 十六进制 + '+'→空格;非法 % 原样;UTF-8 字节直通)。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 参数齐即导入(首版无前端确认弹窗):解析 → 校验 → providers::create 落库。
/// 成功/失败均唤起主窗口;错误以 [deeplink] 前缀打到运行日志(前端无事件通道,首版不做 toast)。
fn handle_deeplink(app: &AppHandle, url: &tauri::Url) -> Result<(), String> {
    let p = parse_import_url(url.as_str())?;
    let st = app.state::<server::AppState>();
    let input = crate::providers::ProviderInput {
        name: p.name,
        base_url: p.base_url,
        api_key: p.api_key,
        model: p.model.unwrap_or_default(),
        wire_api: p
            .wire
            .as_deref()
            .and_then(crate::providers::WireApi::parse)
            .unwrap_or_default(),
        ..Default::default()
    };
    match crate::providers::create(&st.providers_path, input) {
        Ok(provider) => {
            eprintln!(
                "[deeplink] 导入成功: {} ({}), imported:true",
                provider.name, provider.id
            );
            show_main_window(app);
            Ok(())
        }
        Err(errs) => {
            let msg = crate::providers::format_errors(&errs);
            eprintln!("[deeplink] 导入失败: {msg}");
            show_main_window(app);
            Err(msg)
        }
    }
}

fn main() {
    let codex_home = codex_home();
    let config_path = codex_home.join("config.toml");
    let backup_dir = codex_home.join("config-backups");
    let providers_path = codex_home.join("providers.json");

    std::fs::create_dir_all(&backup_dir).ok();

    // 网关固定监听 127.0.0.1:8787（契约要求：Codex 的 config.toml 里 custom.base_url 指向此地址）
    let listener = TcpListener::bind("127.0.0.1:8787")
        .expect("无法绑定 127.0.0.1:8787（端口可能被占用，请先释放后重试）");
    // tokio::from_std 要求非阻塞 socket（否则 panic，tokio#7172）
    listener.set_nonblocking(true).expect("set_nonblocking");
    let app_url = "http://127.0.0.1:8787".to_string();

    // M8:启动器状态 → 先清扫崩溃残留(只清带 launcher.json 标记的目录),再起后台退出监控
    let launcher_state = std::sync::Arc::new(launcher::LauncherState::default());
    launcher::sweep_orphans();
    launcher::spawn_monitor(launcher_state.clone());

    // 阶段 4:加速线路装配——启动即加载线路填入健康状态;accel 配置从 2xapi-settings.json 读入
    let lines = crate::acclines::load_lines(&codex_home);
    let health_state = std::sync::Arc::new(crate::acclines::HealthState::new(lines.lines));
    let accel_state =
        std::sync::Arc::new(std::sync::Mutex::new(server::load_accel_cfg(&codex_home)));
    // 星图 任务 B:每账号节点凭证表(兼容迁移旧单对象 → legacy)
    let nodecreds_store =
        std::sync::Arc::new(std::sync::RwLock::new(nodecreds::load_store(&codex_home)));
    // 托盘「网关开/关」内存态:默认开;托盘与网关守卫共用同一 Arc
    let tray_gate_enabled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));

    let state = server::AppState {
        keypool: std::sync::Arc::new(crate::keypool::KeyPool::new()),
        config_path: config_path.clone(),
        backup_dir: backup_dir.clone(),
        providers_path: providers_path.clone(),
        codex_home: codex_home.clone(),
        // workbuddy 双载体(~/.codebuddy 与 ~/.workbuddy)的公共根;测试传 tempdir(server/gateway 测试态)
        wb_home: std::path::PathBuf::from(
            std::env::var("HOME")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| std::env::var("USERPROFILE").ok())
                .unwrap_or_default(),
        ),
        hermes_home: crate::agents::hermes::hermes_home(),
        // gemini 载体根(~/.gemini 所在);测试传 tempdir
        gem_home: std::path::PathBuf::from(
            std::env::var("HOME")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| std::env::var("USERPROFILE").ok())
                .unwrap_or_default(),
        ),
        grok_home: crate::grok_config::default_grok_home(),
        oc_home: std::path::PathBuf::from(
            std::env::var("HOME")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| std::env::var("USERPROFILE").ok())
                .unwrap_or_default(),
        ),
        oclaw_home: {
            let home = std::env::var("HOME")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| std::env::var("USERPROFILE").ok())
                .unwrap_or_default();
            std::path::PathBuf::from(home).join(".openclaw")
        },
        // Claude Desktop 配置父目录:macOS=~/Library/Application Support,Windows=%APPDATA%。
        cd_home: {
            if cfg!(windows) {
                std::path::PathBuf::from(std::env::var("APPDATA").unwrap_or_else(|_| {
                    std::env::var("USERPROFILE")
                        .map(|home| format!(r"{home}\AppData\Roaming"))
                        .unwrap_or_default()
                }))
            } else if cfg!(target_os = "macos") {
                std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                    .join("Library")
                    .join("Application Support")
            } else {
                std::path::PathBuf::from(std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| {
                    std::env::var("HOME")
                        .map(|home| format!("{home}/.config"))
                        .unwrap_or_default()
                }))
            }
        },
        // Cursor 生态管理(A 段):~/.cursor 所在根(eco adapter join(".cursor/mcp.json"))
        cursor_home: std::path::PathBuf::from(
            std::env::var("HOME")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| std::env::var("USERPROFILE").ok())
                .unwrap_or_default(),
        ),
        launcher: launcher_state,
        health: health_state.clone(),
        accel: accel_state,
        nodecreds: nodecreds_store,
        tray_gate_enabled,
    };

    // 托盘/deeplink 需要访问 AppState(providers_path/网关开关),manage 一份共享克隆
    let state_for_app = state.clone();
    let router = server::build_router(state);

    // Start HTTP server in a dedicated thread with its own tokio runtime
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            // 阶段 4:后台健康探测循环(每 30s 快照 HealthState.lines 探测;线路可经 set_lines 更新)。
            // spawn_health_loop 内部自行 tokio::spawn,此处直接调用即可。
            crate::acclines::spawn_health_loop(
                health_state.clone(),
                std::time::Duration::from_secs(30),
            );
            // 任务书 §五:远程线路表刷新(启动即拉 + 每 60min;accel-remote.json
            // 未配置时静默跳过,不影响内置/缓存表)。
            crate::acclines::spawn_refresh_loop(
                health_state.clone(),
                codex_home.clone(),
                std::time::Duration::from_secs(3600),
            );
            axum::serve(listener, router).await.unwrap();
        });
    });

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(move |app| {
            updater::init(app.handle().clone()).map_err(std::io::Error::other)?;
            app.manage(state_for_app);

            let window = WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::External(app_url.parse().unwrap()),
            )
            .title("2xapi Codex Console")
            .inner_size(1000.0, 720.0)
            .min_inner_size(800.0, 600.0)
            .build()?;

            // 关窗口 → 隐藏而非退出（保持网关 8787 常驻；从托盘重新显示/退出）
            let wh = window.clone();
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = wh.hide();
                }
            });

            // 托盘菜单(竞品吸收 1.1-2):当前供应商/切换供应商/网关开/关/打开主界面/退出
            let app_state = app.state::<server::AppState>();
            let providers = crate::providers::load(&app_state.providers_path);
            let menu = build_tray_menu(
                app.handle(),
                &providers.providers,
                providers.active_provider_id.as_deref(),
                app_state.tray_gate_enabled.load(Ordering::Relaxed),
            )?;
            let tray = TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().cloned().unwrap())
                .menu(&menu)
                .tooltip("2xapi Codex Console（关窗口不退出，网关保持运行）")
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main_window(app),
                    "quit" => app.exit(0),
                    // 网关开/关:翻转内存态(默认开),重建菜单同步标签
                    "gate" => {
                        let st = app.state::<server::AppState>();
                        st.tray_gate_enabled.fetch_xor(true, Ordering::Relaxed);
                        rebuild_tray(app);
                    }
                    // 当前供应商:切到下一个(末位回绕)
                    "active" => {
                        let st = app.state::<server::AppState>();
                        let data = crate::providers::load(&st.providers_path);
                        if let Some(next) =
                            next_provider_id(&data.providers, data.active_provider_id.as_deref())
                        {
                            crate::providers::set_active(&st.providers_path, &next);
                            rebuild_tray(app);
                        }
                    }
                    // 切换供应商子菜单:选中即 set_active 并重建
                    _ => {
                        if let Some(pid) = event.id.as_ref().strip_prefix("provider:") {
                            let st = app.state::<server::AppState>();
                            crate::providers::set_active(&st.providers_path, pid);
                            rebuild_tray(app);
                        }
                    }
                })
                .build(app)?;
            app.manage(TrayState {
                icon: std::sync::Mutex::new(tray),
            });

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    // deeplink 一键导入(竞品吸收 1.1-5):2xapi:// 协议由 Info.plist 注册,系统唤起后走 Opened 事件。
    // 注:dev(cargo run)无 .app 包无协议注册,仅打包后生效;2xapi 站点生成的链接直达这里。
    app.run(|app_handle, event| {
        // cfg 与 tauri 的 RunEvent::Opened 同门控:该变体仅 macos/ios/android 存在
        //(Windows 无此事件;Mac 全绿零覆盖的 cfg 分支教训,CI 三平台矩阵是唯一防线)
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "android"))]
        if let tauri::RunEvent::Opened { urls } = event {
            for url in urls {
                if let Err(e) = handle_deeplink(app_handle, &url) {
                    eprintln!("[deeplink] {e}");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(id: &str, name: &str) -> crate::providers::Provider {
        crate::providers::Provider {
            id: id.into(),
            name: name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn tray_items_labels_and_structure() {
        let provs = vec![p("a", "供应商A"), p("b", "供应商B")];
        let items = tray_items(&provs, Some("a"), true);
        assert_eq!(items.len(), 5);
        assert_eq!(items[0].id, "active");
        assert_eq!(items[0].label, "当前供应商:供应商A");
        assert_eq!(items[1].id, "providers");
        assert_eq!(items[1].label, "切换供应商");
        match &items[1].kind {
            TraySpecKind::Submenu(e) => assert_eq!(
                e,
                &vec![
                    ("a".to_string(), "供应商A".to_string()),
                    ("b".to_string(), "供应商B".to_string())
                ]
            ),
            _ => panic!("切换供应商应为子菜单"),
        }
        assert_eq!(items[2].label, "网关:开");
        assert_eq!(items[3].label, "打开主界面");
        assert_eq!(items[4].label, "退出(关闭网关)");
        // 无 active / 网关关 → 标签联动
        let items = tray_items(&provs, None, false);
        assert_eq!(items[0].label, "当前供应商:未设置");
        assert_eq!(items[2].label, "网关:关");
    }

    #[test]
    fn next_provider_id_cycles_and_wraps() {
        let provs = vec![p("a", "A"), p("b", "B"), p("c", "C")];
        assert_eq!(next_provider_id(&provs, Some("a")), Some("b".to_string()));
        assert_eq!(next_provider_id(&provs, Some("c")), Some("a".to_string())); // 末位回绕
        assert_eq!(next_provider_id(&provs, Some("zzz")), Some("a".to_string())); // 不在列表 → 首个
        assert_eq!(next_provider_id(&provs, None), Some("a".to_string()));
        assert_eq!(next_provider_id(&[], None), None);
    }

    #[test]
    fn parse_import_url_valid() {
        let p = parse_import_url(
            "2xapi://import?name=%E6%88%91%E7%9A%84%E4%B8%AD%E8%BD%AC&baseUrl=https%3A%2F%2Fapi.example.com%2Fv1&apiKey=sk-123&model=gpt-4o&wire=chat_completions",
        )
        .unwrap();
        assert_eq!(p.name, "我的中转");
        assert_eq!(p.base_url, "https://api.example.com/v1");
        assert_eq!(p.api_key, "sk-123");
        assert_eq!(p.model.as_deref(), Some("gpt-4o"));
        assert_eq!(p.wire.as_deref(), Some("chat_completions"));
        // 缺省参数 → None(create 的 validate 再兜底 model 必填)
        let p = parse_import_url("2xapi://import?name=a&baseUrl=https://x&apiKey=k").unwrap();
        assert_eq!(p.model, None);
        assert_eq!(p.wire, None);
    }

    #[test]
    fn parse_import_url_rejects_bad_input() {
        assert!(parse_import_url("https://example.com").is_err()); // 协议错
        assert!(parse_import_url("2xapi://other?name=a&baseUrl=https://x&apiKey=k").is_err()); // 非 import 操作
        assert!(parse_import_url("2xapi://import?baseUrl=https://x&apiKey=k").is_err()); // 缺 name
        assert!(parse_import_url("2xapi://import?name=a&apiKey=k").is_err()); // 缺 baseUrl
        assert!(parse_import_url("2xapi://import?name=a&baseUrl=ftp://x&apiKey=k").is_err()); // 非 http(s)
        assert!(parse_import_url("2xapi://import?name=a&baseUrl=https://x").is_err()); // 缺 apiKey
        assert!(parse_import_url("not a url").is_err()); // 无效 URL
    }
}
