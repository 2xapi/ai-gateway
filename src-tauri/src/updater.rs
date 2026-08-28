use serde::Serialize;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
};
use tauri::AppHandle;
use tauri_plugin_updater::UpdaterExt;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub current: String,
    pub latest: String,
    pub update_available: bool,
    pub notes: Option<String>,
    pub pub_date: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub state: String,
    pub version: Option<String>,
    pub downloaded: u64,
    pub total: Option<u64>,
    pub error: Option<String>,
}

impl Default for UpdateStatus {
    fn default() -> Self {
        Self {
            state: "idle".into(),
            version: None,
            downloaded: 0,
            total: None,
            error: None,
        }
    }
}

#[derive(Default)]
pub struct UpdateState {
    running: AtomicBool,
    status: Mutex<UpdateStatus>,
}

static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();
static UPDATE_STATE: OnceLock<Arc<UpdateState>> = OnceLock::new();

pub fn init(app: AppHandle) -> Result<(), String> {
    APP_HANDLE
        .set(app)
        .map_err(|_| "应用句柄已初始化".to_string())?;
    UPDATE_STATE
        .set(Arc::new(UpdateState::default()))
        .map_err(|_| "更新状态已初始化".to_string())
}

pub fn app_handle() -> Option<AppHandle> {
    APP_HANDLE.get().cloned()
}

pub fn status() -> UpdateStatus {
    UPDATE_STATE
        .get()
        .map(|state| state.snapshot())
        .unwrap_or_default()
}

impl UpdateState {
    pub fn snapshot(&self) -> UpdateStatus {
        self.status.lock().unwrap().clone()
    }

    fn replace(&self, status: UpdateStatus) {
        *self.status.lock().unwrap() = status;
    }

    fn fail(&self, error: String) {
        let version = self.status.lock().unwrap().version.clone();
        self.replace(UpdateStatus {
            state: "error".into(),
            version,
            downloaded: 0,
            total: None,
            error: Some(error),
        });
        self.running.store(false, Ordering::Release);
    }
}

pub async fn check(app: &AppHandle) -> Result<UpdateInfo, String> {
    let update = app
        .updater_builder()
        .build()
        .map_err(|error| format!("初始化更新器失败: {error}"))?
        .check()
        .await
        .map_err(|error| format!("检查更新失败: {error}"))?;

    Ok(match update {
        Some(update) => UpdateInfo {
            current: update.current_version,
            latest: update.version,
            update_available: true,
            notes: update.body,
            pub_date: update.date.map(|date| date.to_string()),
        },
        None => UpdateInfo {
            current: env!("CARGO_PKG_VERSION").into(),
            latest: env!("CARGO_PKG_VERSION").into(),
            update_available: false,
            notes: None,
            pub_date: None,
        },
    })
}

pub fn start_install(app: AppHandle) -> Result<(), String> {
    let state = UPDATE_STATE
        .get()
        .cloned()
        .ok_or_else(|| "更新器尚未初始化".to_string())?;
    state
        .running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| "更新任务正在运行".to_string())?;
    state.replace(UpdateStatus {
        state: "checking".into(),
        ..UpdateStatus::default()
    });

    tauri::async_runtime::spawn(async move {
        if let Err(error) = install(app, state.clone()).await {
            state.fail(error);
        }
    });
    Ok(())
}

async fn install(app: AppHandle, state: Arc<UpdateState>) -> Result<(), String> {
    let updater = app
        .updater_builder()
        .build()
        .map_err(|error| format!("初始化更新器失败: {error}"))?;
    let Some(update) = updater
        .check()
        .await
        .map_err(|error| format!("检查更新失败: {error}"))?
    else {
        state.replace(UpdateStatus::default());
        state.running.store(false, Ordering::Release);
        return Ok(());
    };

    let version = update.version.clone();
    state.replace(UpdateStatus {
        state: "downloading".into(),
        version: Some(version.clone()),
        downloaded: 0,
        total: None,
        error: None,
    });

    let progress = state.clone();
    let mut downloaded = 0_u64;
    let bytes = update
        .download(
            move |chunk_len, total| {
                downloaded = downloaded.saturating_add(chunk_len as u64);
                progress.replace(UpdateStatus {
                    state: "downloading".into(),
                    version: Some(version.clone()),
                    downloaded,
                    total,
                    error: None,
                });
            },
            || {},
        )
        .await
        .map_err(|error| format!("下载或签名校验失败: {error}"))?;

    state.replace(UpdateStatus {
        state: "installing".into(),
        version: Some(update.version.clone()),
        downloaded: bytes.len() as u64,
        total: Some(bytes.len() as u64),
        error: None,
    });
    update
        .install(bytes)
        .map_err(|error| format!("安装更新失败: {error}"))?;

    state.replace(UpdateStatus {
        state: "restarting".into(),
        version: Some(update.version),
        downloaded: 0,
        total: None,
        error: None,
    });
    app.restart();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_status_defaults_to_idle() {
        let state = UpdateState::default();
        let status = state.snapshot();
        assert_eq!(status.state, "idle");
        assert_eq!(status.downloaded, 0);
        assert!(status.error.is_none());
    }

    #[test]
    fn failed_update_releases_single_flight_lock() {
        let state = UpdateState::default();
        state.running.store(true, Ordering::Release);
        state.fail("network error".into());
        assert!(!state.running.load(Ordering::Acquire));
        assert_eq!(state.snapshot().state, "error");
    }
}
