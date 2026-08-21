//! 可选用量悬浮窗的配置与原生窗口动作契约。
//!
//! 主线程通过 `register_app_handle` 和 `apply_window_settings` 接入本模块。透明度、
//! 悬停恢复和刷新周期通过悬浮窗页面脚本同步；Tauri 2 的原生窗口 API 不提供通用的
//! 窗口 opacity setter。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::Duration;
use tauri::Manager;

#[allow(dead_code)]
static APP_HANDLE: OnceLock<tauri::AppHandle> = OnceLock::new();
static SETTINGS_SAVE_LOCK: Mutex<()> = Mutex::new(());
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);
#[allow(dead_code)]
static POSITION_LISTENER_REGISTERED: AtomicBool = AtomicBool::new(false);

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct PositionUpdate {
    codex_home: PathBuf,
    position: OverlayPosition,
}

#[allow(dead_code)]
static POSITION_WRITER: OnceLock<mpsc::Sender<PositionUpdate>> = OnceLock::new();

pub(crate) fn settings_write_lock() -> Result<std::sync::MutexGuard<'static, ()>, String> {
    SETTINGS_SAVE_LOCK
        .lock()
        .map_err(|_| "设置保存锁已损坏".to_string())
}

#[allow(dead_code)]
pub fn register_app_handle(handle: tauri::AppHandle) {
    let _ = APP_HANDLE.set(handle);
}

#[allow(dead_code)]
pub fn with_window<F>(action: F) -> Result<(), String>
where
    F: FnOnce(&tauri::WebviewWindow) -> Result<(), String>,
{
    let handle = APP_HANDLE
        .get()
        .ok_or_else(|| "悬浮窗尚未初始化".to_string())?;
    let window = handle
        .get_webview_window(WINDOW_LABEL)
        .ok_or_else(|| "悬浮窗窗口不存在".to_string())?;
    action(&window)
}

pub const WINDOW_LABEL: &str = "usage-overlay";
pub const MIN_OPACITY: f64 = 0.60;
pub const MAX_OPACITY: f64 = 1.00;
pub const DEFAULT_OPACITY: f64 = 0.88;
pub const MIN_REFRESH_INTERVAL_SECS: u64 = 15;
pub const MAX_REFRESH_INTERVAL_SECS: u64 = 300;
pub const DEFAULT_REFRESH_INTERVAL_SECS: u64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OverlayPosition {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsageOverlaySettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_opacity")]
    pub opacity: f64,
    #[serde(default = "default_true")]
    pub always_on_top: bool,
    #[serde(default)]
    pub click_through: bool,
    #[serde(default = "default_true")]
    pub restore_full_opacity_on_hover: bool,
    #[serde(default = "default_refresh_interval_secs")]
    pub refresh_interval_secs: u64,
    #[serde(default)]
    pub position: Option<OverlayPosition>,
}

impl Default for UsageOverlaySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            opacity: DEFAULT_OPACITY,
            always_on_top: true,
            click_through: false,
            restore_full_opacity_on_hover: true,
            refresh_interval_secs: DEFAULT_REFRESH_INTERVAL_SECS,
            position: None,
        }
    }
}

fn default_opacity() -> f64 {
    DEFAULT_OPACITY
}

fn default_true() -> bool {
    true
}

fn default_refresh_interval_secs() -> u64 {
    DEFAULT_REFRESH_INTERVAL_SECS
}

impl UsageOverlaySettings {
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        if !self.opacity.is_finite() || !(MIN_OPACITY..=MAX_OPACITY).contains(&self.opacity) {
            errors.push(format!(
                "opacity 必须在 {MIN_OPACITY:.2}..{MAX_OPACITY:.2} 之间"
            ));
        }
        if !(MIN_REFRESH_INTERVAL_SECS..=MAX_REFRESH_INTERVAL_SECS)
            .contains(&self.refresh_interval_secs)
        {
            errors.push(format!(
                "refreshIntervalSecs 必须在 {MIN_REFRESH_INTERVAL_SECS}..{MAX_REFRESH_INTERVAL_SECS} 秒之间"
            ));
        }
        if let Some(position) = &self.position {
            if position.x == i32::MIN || position.y == i32::MIN {
                errors.push("position.x 和 position.y 不能使用 i32::MIN".into());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// 计算悬停时应使用的内容透明度。
///
/// 原生鼠标穿透开启后，窗口不会收到悬停事件，因此此时不能承诺移入恢复不透明。
pub fn effective_opacity(settings: &UsageOverlaySettings, hovered: bool) -> f64 {
    if hovered && settings.restore_full_opacity_on_hover && !settings.click_through {
        MAX_OPACITY
    } else {
        settings.opacity
    }
}

fn settings_path(codex_home: &Path) -> PathBuf {
    codex_home.join("2xapi-settings.json")
}

/// 读取悬浮窗设置。文件或段不存在时使用默认值；存在但格式/范围错误时返回错误。
pub fn load_settings(codex_home: &Path) -> Result<UsageOverlaySettings, String> {
    let path = settings_path(codex_home);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(UsageOverlaySettings::default())
        }
        Err(error) => return Err(format!("读取设置失败: {error}")),
    };
    let root: Value =
        serde_json::from_str(&raw).map_err(|error| format!("设置 JSON 无效: {error}"))?;
    let object = root
        .as_object()
        .ok_or_else(|| "2xapi-settings.json 顶层必须是对象".to_string())?;
    let settings: UsageOverlaySettings = object
        .get("usageOverlay")
        .map(|value| {
            serde_json::from_value(value.clone())
                .map_err(|error| format!("usageOverlay 格式无效: {error}"))
        })
        .transpose()?
        .unwrap_or_default();
    settings.validate().map_err(|errors| errors.join("；"))?;
    Ok(settings)
}

/// 保存悬浮窗设置，保留 `2xapi-settings.json` 顶层其他字段。
pub fn save_settings(codex_home: &Path, settings: &UsageOverlaySettings) -> Result<(), String> {
    settings.validate().map_err(|errors| errors.join("；"))?;
    let _save_guard = settings_write_lock()?;
    write_settings(codex_home, settings)
}

/// save_settings 的锁内实现:调用方须已持有 `settings_write_lock`,避免二次加锁死锁。
fn write_settings(codex_home: &Path, settings: &UsageOverlaySettings) -> Result<(), String> {
    std::fs::create_dir_all(codex_home).map_err(|error| format!("创建配置目录失败: {error}"))?;
    let path = settings_path(codex_home);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("读取设置失败: {error}")),
    };
    let mut root = if raw.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str::<Value>(&raw).map_err(|error| format!("设置 JSON 无效: {error}"))?
    };
    let mut persisted_settings = settings.clone();
    if persisted_settings.position.is_none() {
        persisted_settings.position = existing_position(&root);
    }
    let object = root
        .as_object_mut()
        .ok_or_else(|| "2xapi-settings.json 顶层必须是对象".to_string())?;
    object.insert(
        "usageOverlay".into(),
        serde_json::to_value(persisted_settings)
            .map_err(|error| format!("序列化悬浮窗设置失败: {error}"))?,
    );
    let encoded =
        serde_json::to_vec_pretty(&root).map_err(|error| format!("序列化设置失败: {error}"))?;
    let temp = temporary_settings_path(&path);
    let result = write_and_replace(&path, &temp, &encoded);
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn existing_position(root: &Value) -> Option<OverlayPosition> {
    let value = root.get("usageOverlay")?.get("position")?;
    let position = serde_json::from_value::<Option<OverlayPosition>>(value.clone()).ok()?;
    position.filter(|position| position.x != i32::MIN && position.y != i32::MIN)
}

fn temporary_settings_path(path: &Path) -> PathBuf {
    let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(
        ".2xapi-settings.{}.{}.tmp",
        std::process::id(),
        counter
    ))
}

fn write_and_replace(path: &Path, temp: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::fs::OpenOptions;
    use std::io::Write;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp)
        .map_err(|error| format!("创建临时设置失败: {error}"))?;
    file.write_all(bytes)
        .map_err(|error| format!("写临时设置失败: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("同步临时设置失败: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temp, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("设置临时设置权限失败: {error}"))?;
    }
    replace_file(temp, path)?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(temp: &Path, path: &Path) -> Result<(), String> {
    std::fs::rename(temp, path).map_err(|error| format!("替换设置失败: {error}"))
}

#[cfg(windows)]
fn replace_file(temp: &Path, path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

    let source: Vec<u16> = temp
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let destination: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let replaced = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        Err(format!("替换设置失败: {}", std::io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn MoveFileExW(existing_file_name: *const u16, new_file_name: *const u16, flags: u32) -> i32;
}

#[allow(dead_code)]
fn position_writer() -> &'static mpsc::Sender<PositionUpdate> {
    POSITION_WRITER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<PositionUpdate>();
        let _ = std::thread::Builder::new()
            .name("2xapi-overlay-position".into())
            .spawn(move || {
                while let Ok(mut update) = receiver.recv() {
                    loop {
                        match receiver.recv_timeout(Duration::from_millis(250)) {
                            Ok(next) => update = next,
                            Err(mpsc::RecvTimeoutError::Timeout) => break,
                            Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    persist_position(update);
                }
            });
        sender
    })
}

#[allow(dead_code)]
/// 落盘窗口位置:在写锁内重读磁盘最新设置,只更新 position,其余字段(含 enabled)
/// 以磁盘原值为准。避免后台 250ms 合流线程拿着旧快照(可能 enabled=true)落盘,
/// 覆盖 CloseRequested 刚保存的 enabled=false。
fn persist_position(update: PositionUpdate) {
    let Ok(_guard) = settings_write_lock() else {
        return;
    };
    let Ok(mut settings) = load_settings(&update.codex_home) else {
        return;
    };
    if settings.position.as_ref() == Some(&update.position) {
        return;
    }
    settings.position = Some(update.position);
    let _ = write_settings(&update.codex_home, &settings);
}

#[allow(dead_code)]
fn codex_home_path() -> PathBuf {
    let home = std::env::var("HOME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| std::env::var("USERPROFILE").ok())
        .unwrap_or_default();
    PathBuf::from(std::env::var("CODEX_HOME").unwrap_or_else(|_| format!("{home}/.codex")))
}

#[allow(dead_code)]
fn register_position_listener<R: tauri::Runtime>(window: &tauri::WebviewWindow<R>) {
    if POSITION_LISTENER_REGISTERED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    let codex_home = codex_home_path();
    let sender = position_writer().clone();
    let window_for_close = window.clone();
    let codex_home_for_close = codex_home.clone();
    window.on_window_event(move |event| match event {
        tauri::WindowEvent::Moved(position) => {
            let _ = sender.send(PositionUpdate {
                codex_home: codex_home.clone(),
                position: OverlayPosition {
                    x: position.x,
                    y: position.y,
                },
            });
        }
        tauri::WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            let _ = window_for_close.hide();
            if let Ok(mut settings) = load_settings(&codex_home_for_close) {
                settings.enabled = false;
                let _ = save_settings(&codex_home_for_close, &settings);
            }
        }
        _ => {}
    });
}

#[allow(dead_code)]
fn resolve_saved_position<R: tauri::Runtime>(
    window: &tauri::WebviewWindow<R>,
    position: OverlayPosition,
) -> tauri::PhysicalPosition<i32> {
    let requested = tauri::PhysicalPosition::new(position.x, position.y);
    let Ok(monitors) = window.available_monitors() else {
        return requested;
    };
    if monitors.is_empty() {
        return requested;
    }
    let size = window
        .outer_size()
        .unwrap_or_else(|_| tauri::PhysicalSize::new(320, 190));
    if monitors
        .iter()
        .any(|monitor| position_has_visible_area(requested, size, monitor))
    {
        return requested;
    }
    let monitor = window
        .primary_monitor()
        .ok()
        .flatten()
        .or_else(|| monitors.first().cloned());
    let Some(monitor) = monitor else {
        return requested;
    };
    let work_area = monitor.work_area();
    let x = work_area.position.x as i64
        + (i64::from(work_area.size.width)
            .saturating_sub(i64::from(size.width))
            .max(0)
            / 2);
    let y = work_area.position.y as i64
        + (i64::from(work_area.size.height)
            .saturating_sub(i64::from(size.height))
            .max(0)
            / 2);
    tauri::PhysicalPosition::new(clamp_i32(x), clamp_i32(y))
}

#[allow(dead_code)]
fn position_has_visible_area(
    position: tauri::PhysicalPosition<i32>,
    size: tauri::PhysicalSize<u32>,
    monitor: &tauri::Monitor,
) -> bool {
    let area = monitor.work_area();
    let left = i64::from(position.x);
    let top = i64::from(position.y);
    let right = left.saturating_add(i64::from(size.width.max(1)));
    let bottom = top.saturating_add(i64::from(size.height.max(1)));
    let area_left = i64::from(area.position.x);
    let area_top = i64::from(area.position.y);
    let area_right = area_left.saturating_add(i64::from(area.size.width));
    let area_bottom = area_top.saturating_add(i64::from(area.size.height));
    let visible_width = right.min(area_right) - left.max(area_left);
    let visible_height = bottom.min(area_bottom) - top.max(area_top);
    visible_width >= i64::from(size.width.clamp(1, 48))
        && visible_height >= i64::from(size.height.clamp(1, 32))
}

#[allow(dead_code)]
fn clamp_i32(value: i64) -> i32 {
    value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

#[allow(dead_code)]
fn webview_settings_script(settings: &UsageOverlaySettings) -> String {
    let base_opacity = effective_opacity(settings, false);
    let hover_opacity = effective_opacity(settings, true);
    let restore_on_hover = settings.restore_full_opacity_on_hover && !settings.click_through;
    let refresh_interval_ms = settings.refresh_interval_secs.saturating_mul(1000);
    format!(
        r#"(function() {{
  const config = {{
    baseOpacity: {base_opacity},
    hoverOpacity: {hover_opacity},
    restoreOnHover: {restore_on_hover},
    refreshIntervalMs: {refresh_interval_ms}
  }};
  const state = window.__twoXapiUsageOverlay || (window.__twoXapiUsageOverlay = {{
    timer: null,
    callback: null,
    hovered: false,
    installed: false,
    observer: null,
    nativeSetInterval: null,
    nativeClearInterval: null
  }});
  state.config = config;
  if (!state.installed) {{
    state.nativeSetInterval = window.setInterval.bind(window);
    state.nativeClearInterval = window.clearInterval.bind(window);
    window.setInterval = function(callback, delay) {{
      const source = typeof callback === "function"
        ? Function.prototype.toString.call(callback)
        : "";
      if (!source.includes("refreshOverlay")) {{
        return state.nativeSetInterval(callback, delay);
      }}
      if (state.timer !== null) state.nativeClearInterval(state.timer);
      state.callback = callback;
      state.timer = state.nativeSetInterval(callback, state.config.refreshIntervalMs);
      return state.timer;
    }};
    state.installed = true;
  }}
  if (state.callback !== null && state.timer !== null) {{
    state.nativeClearInterval(state.timer);
    state.timer = state.nativeSetInterval(state.callback, state.config.refreshIntervalMs);
  }}
  function applyRoot() {{
    const root = document.getElementById("usageOverlayRoot");
    if (!root) return;
    root.style.opacity = String(
      state.hovered && state.config.restoreOnHover
        ? state.config.hoverOpacity
        : state.config.baseOpacity
    );
    if (root.__twoXapiHoverBound) return;
    root.__twoXapiHoverBound = true;
    root.addEventListener("mouseenter", function() {{
      state.hovered = true;
      applyRoot();
    }});
    root.addEventListener("mouseleave", function() {{
      state.hovered = false;
      applyRoot();
    }});
  }}
  applyRoot();
  if (document.readyState === "loading") {{
    document.addEventListener("DOMContentLoaded", applyRoot);
  }}
  if (!state.observer && window.MutationObserver && document.documentElement) {{
    state.observer = new MutationObserver(applyRoot);
    state.observer.observe(document.documentElement, {{ childList: true, subtree: true }});
  }}
}})();"#,
        base_opacity = base_opacity,
        hover_opacity = hover_opacity,
        restore_on_hover = restore_on_hover,
        refresh_interval_ms = refresh_interval_ms,
    )
}

#[allow(dead_code)]
fn apply_webview_settings<R: tauri::Runtime>(
    window: &tauri::WebviewWindow<R>,
    settings: &UsageOverlaySettings,
) {
    let _ = window.eval(webview_settings_script(settings));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum WindowAction {
    Show,
    Hide,
    Toggle,
    ApplySettings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum WindowVisibility {
    Hidden,
    Visible,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub struct WindowState {
    pub visibility: WindowVisibility,
    pub expanded: bool,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            visibility: WindowVisibility::Hidden,
            expanded: false,
        }
    }
}

#[allow(dead_code)]
pub fn next_visibility(action: WindowAction, current: WindowVisibility) -> WindowVisibility {
    match action {
        WindowAction::Show => WindowVisibility::Visible,
        WindowAction::Hide => WindowVisibility::Hidden,
        WindowAction::Toggle => match current {
            WindowVisibility::Visible => WindowVisibility::Hidden,
            WindowVisibility::Hidden => WindowVisibility::Visible,
            WindowVisibility::Unknown => WindowVisibility::Visible,
        },
        WindowAction::ApplySettings => current,
    }
}

/// 将配置中的原生窗口行为应用到已创建的 Tauri 2 窗口。
/// 透明度仍由窗口页面 CSS 使用 `settings.opacity` 应用。
#[allow(dead_code)]
pub fn apply_window_settings<R: tauri::Runtime>(
    window: &tauri::WebviewWindow<R>,
    settings: &UsageOverlaySettings,
) -> Result<(), String> {
    settings.validate().map_err(|errors| errors.join("；"))?;
    register_position_listener(window);
    apply_webview_settings(window, settings);
    window
        .set_always_on_top(settings.always_on_top)
        .map_err(|error| format!("设置悬浮窗置顶失败: {error}"))?;
    window
        .set_ignore_cursor_events(settings.click_through)
        .map_err(|error| format!("设置悬浮窗鼠标穿透失败: {error}"))?;
    if let Some(position) = settings.position.as_ref() {
        let requested = tauri::PhysicalPosition::new(position.x, position.y);
        let resolved = resolve_saved_position(window, position.clone());
        window
            .set_position(tauri::Position::Physical(resolved))
            .map_err(|error| format!("恢复悬浮窗位置失败: {error}"))?;
        if resolved != requested {
            let _ = position_writer().send(PositionUpdate {
                codex_home: codex_home_path(),
                position: OverlayPosition {
                    x: resolved.x,
                    y: resolved.y,
                },
            });
        }
    }
    if settings.enabled {
        window
            .show()
            .map_err(|error| format!("显示悬浮窗失败: {error}"))?;
    } else {
        window
            .hide()
            .map_err(|error| format!("隐藏悬浮窗失败: {error}"))?;
    }
    Ok(())
}
