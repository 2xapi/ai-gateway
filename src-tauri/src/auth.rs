use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::OnceLock;

const MANAGEMENT_URL: &str = "https://2xapi.com/api/v1";
const MANAGEMENT_FALLBACK: &str = "https://2xa.cc.cd/api/v1";

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub user: Value,
    #[serde(default = "default_auto_refresh")]
    pub auto_refresh: bool,
}

fn default_auto_refresh() -> bool {
    false
}

fn session_path(codex_home: &Path) -> std::path::PathBuf {
    codex_home.join("2xapi-session.json")
}
fn remembered_path(codex_home: &Path) -> std::path::PathBuf {
    codex_home.join("2xapi-remembered.json")
}

fn write_private(path: &Path, raw: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, raw)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn repair_private_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

pub fn load_session(codex_home: &Path) -> Option<Session> {
    let path = session_path(codex_home);
    let raw = std::fs::read_to_string(&path).ok()?;
    let s: Session = serde_json::from_str(&raw).ok()?;
    repair_private_permissions(&path);
    if s.expires_at > 0 && chrono::Local::now().timestamp_millis() > s.expires_at {
        return None;
    }
    Some(s)
}

/// 「保存登录」核心:session 过期时用 refresh_token 免验证码续期
/// (Sub2API POST /auth/refresh {refresh_token} → data:{access_token, refresh_token, expires_in})。
/// 成功则原地更新 session 文件并返回新 session;refresh_token 也失效则 None(需重新登录)。
pub async fn refresh_session(codex_home: &Path) -> Option<Session> {
    let raw = std::fs::read_to_string(session_path(codex_home)).ok()?;
    let old: Session = serde_json::from_str(&raw).ok()?;
    if old.refresh_token.is_empty() || !old.auto_refresh {
        return None;
    }
    let body = json!({ "refresh_token": old.refresh_token });
    let result = xapi_request("/auth/refresh", reqwest::Method::POST, &body, "")
        .await
        .ok()?;
    let d = result
        .get("data")
        .filter(|v| v.is_object())
        .unwrap_or(&result);
    let access_token = d.get("access_token")?.as_str()?.to_string();
    let refresh_token = d
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or(&old.refresh_token)
        .to_string();
    let expires_in = d.get("expires_in").and_then(|v| v.as_i64()).unwrap_or(3600);
    let session = Session {
        access_token,
        refresh_token,
        expires_at: chrono::Local::now().timestamp_millis() + expires_in * 1000,
        user: old.user.clone(),
        auto_refresh: true,
    };
    let _ = write_private(
        &session_path(codex_home),
        &serde_json::to_string_pretty(&session).unwrap_or_default(),
    );
    Some(session)
}

pub fn save_session(codex_home: &Path, result: &LoginResult, auto_refresh: bool) {
    let session = Session {
        access_token: result.access_token.clone(),
        refresh_token: result.refresh_token.clone(),
        expires_at: chrono::Local::now().timestamp_millis() + result.expires_in * 1000,
        user: result.user.clone(),
        auto_refresh,
    };
    let raw = serde_json::to_string_pretty(&session).unwrap_or_default();
    let _ = write_private(&session_path(codex_home), &raw);
}

pub fn clear_session(codex_home: &Path) {
    let _ = std::fs::remove_file(session_path(codex_home));
}

pub fn load_remembered(codex_home: &Path) -> Option<(String, String)> {
    let path = remembered_path(codex_home);
    let raw = std::fs::read_to_string(&path).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    let email = v
        .get("email")
        .and_then(|e| e.as_str())
        .unwrap_or("")
        .to_string();
    let clean =
        serde_json::to_string_pretty(&json!({ "email": email.clone() })).unwrap_or_default();
    if raw.trim() != clean.trim() {
        let _ = write_private(&path, &clean);
    } else {
        repair_private_permissions(&path);
    }
    if email.is_empty() {
        None
    } else {
        Some((email, String::new()))
    }
}

pub fn save_remembered(codex_home: &Path, email: &str, _password: &str) {
    let raw = serde_json::to_string_pretty(&json!({ "email": email })).unwrap_or_default();
    let _ = write_private(&remembered_path(codex_home), &raw);
}

pub fn clear_remembered(codex_home: &Path) {
    let _ = std::fs::remove_file(remembered_path(codex_home));
}

pub struct LoginResult {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
    pub user: Value,
}

fn api_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(12))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .no_proxy()
            .build()
            .expect("failed to build HTTP client")
    })
}

async fn xapi_request(
    path: &str,
    method: reqwest::Method,
    body: &Value,
    access_token: &str,
) -> Result<Value, String> {
    let urls = [MANAGEMENT_URL, MANAGEMENT_FALLBACK];
    let mut last_err = String::new();
    for base in &urls {
        let url = format!("{}{}", base.trim_end_matches('/'), path);
        let host = base
            .trim_start_matches("https://")
            .split('/')
            .next()
            .unwrap_or(base);
        let mut req = api_client().request(method.clone(), &url);
        if !access_token.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", access_token));
        }
        if body != &json!({}) {
            req = req.json(body);
        }
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                let json: Value = resp.json().await.unwrap_or(json!({}));
                if status.is_success() {
                    return Ok(json);
                }
                let err = json
                    .get("error")
                    .or_else(|| json.get("message"))
                    .and_then(|e| e.as_str())
                    .unwrap_or("unknown error");
                last_err = format!("[{}] {}", host, err);
            }
            Err(e) => last_err = format!("[{}] 连接失败: {}", host, e),
        }
    }
    Err(last_err)
}

/// 验证码设置(Sub2API settings/public 为扁平字段:tencent_captcha_enabled / tencent_captcha_app_id)。
/// 旧实现按嵌套 captcha 段读,恒得 enabled=false —— 已按 Sub2API 实际结构修正。
pub async fn fetch_captcha_settings() -> Result<Value, String> {
    let result = xapi_request(
        "/settings/public?timezone=UTC",
        reqwest::Method::GET,
        &json!({}),
        "",
    )
    .await?;
    let d = result.get("data").unwrap_or(&result);
    let app_id = d
        .get("tencent_captcha_app_id")
        .map(|v| match v {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => String::new(),
        })
        .unwrap_or_default();
    Ok(json!({
        "enabled": d.get("tencent_captcha_enabled").and_then(|v| v.as_bool()).unwrap_or(false),
        "appId": app_id,
    }))
}

pub async fn login(
    email: &str,
    password: &str,
    captcha_ticket: &str,
    captcha_randstr: &str,
) -> Result<LoginResult, String> {
    // Sub2API LoginRequest 字段:tencent_captcha_ticket / tencent_captcha_randstr(源码 auth_handler.go)
    let body = json!({
        "email": email,
        "password": password,
        "tencent_captcha_ticket": captcha_ticket,
        "tencent_captcha_randstr": captcha_randstr,
    });
    let result = xapi_request("/auth/login", reqwest::Method::POST, &body, "").await?;

    if result
        .get("requires_2fa")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Err("requires 2fa".into());
    }

    // Sub2API 响应为 {code, message, data:{access_token, refresh_token, expires_in, user}}(response.Success 封装)
    // —— token 嵌套在 data 内;顶层查找仅作兜底
    let payload = result
        .get("data")
        .filter(|d| d.is_object())
        .unwrap_or(&result);

    let access_token = payload
        .get("access_token")
        .or_else(|| payload.get("accessToken"))
        .and_then(|v| v.as_str())
        .ok_or("登录响应未包含 access token")?
        .to_string();

    let refresh_token = payload
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let expires_in = payload
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .unwrap_or(3600);

    let user = payload.get("user").cloned().unwrap_or(json!({}));

    Ok(LoginResult {
        access_token,
        refresh_token,
        expires_in,
        user,
    })
}

pub async fn fetch_key_groups(access_token: &str) -> Result<Value, String> {
    xapi_request("/groups", reqwest::Method::GET, &json!({}), access_token).await
}

/// 用户 API Key 列表——「一键导入」数据源。
/// 2xapi 部署版为 GET /keys(响应 data.items;main 分支源码的 /api-keys 在该版本 404)。
pub async fn fetch_api_keys(access_token: &str) -> Result<Value, String> {
    xapi_request(
        "/keys?page=1&page_size=100",
        reqwest::Method::GET,
        &json!({}),
        access_token,
    )
    .await
}

/// relay 上游地址(settings.api_base_url)——导入供应商的 baseUrl。
pub async fn fetch_relay_base_url() -> Result<String, String> {
    let result = xapi_request(
        "/settings/public?timezone=UTC",
        reqwest::Method::GET,
        &json!({}),
        "",
    )
    .await?;
    let d = result.get("data").unwrap_or(&result);
    let raw = d
        .get("api_base_url")
        .and_then(|v| v.as_str())
        .unwrap_or("https://2xa.cc.cd");
    Ok(raw
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .trim_end_matches('/')
        .to_string())
}

/// 当前账号信息(Sub2API GET /auth/me,data 含实时 balance/frozen_balance)。
pub async fn fetch_me(access_token: &str) -> Result<Value, String> {
    let result = xapi_request("/auth/me", reqwest::Method::GET, &json!({}), access_token).await?;
    Ok(result.get("data").cloned().unwrap_or(json!({})))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "2xapi-auth-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn remembered_account_never_persists_password() {
        let home = temp_home("remembered");
        save_remembered(&home, "user@example.com", "plain-secret");
        let raw = std::fs::read_to_string(remembered_path(&home)).unwrap();
        assert!(raw.contains("user@example.com"));
        assert!(!raw.contains("plain-secret"));
        assert_eq!(
            load_remembered(&home),
            Some(("user@example.com".into(), String::new()))
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(remembered_path(&home))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn remember_flag_controls_session_auto_refresh() {
        let home = temp_home("session");
        let login = LoginResult {
            access_token: "access-secret".into(),
            refresh_token: "refresh-secret".into(),
            expires_in: 3600,
            user: json!({ "email": "user@example.com" }),
        };
        save_session(&home, &login, false);
        let raw = std::fs::read_to_string(session_path(&home)).unwrap();
        let session: Session = serde_json::from_str(&raw).unwrap();
        assert!(!session.auto_refresh);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(session_path(&home))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn legacy_remembered_password_is_scrubbed_on_load() {
        let home = temp_home("legacy-remembered");
        let path = remembered_path(&home);
        std::fs::write(
            &path,
            r#"{"email":"legacy@example.com","password":"legacy-plain-password"}"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        assert_eq!(
            load_remembered(&home),
            Some(("legacy@example.com".into(), String::new()))
        );
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("legacy-plain-password"));
        assert!(!raw.contains("password"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn legacy_session_without_remember_flag_fails_closed() {
        let home = temp_home("legacy-session");
        let path = session_path(&home);
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "access_token": "access-secret",
                "refresh_token": "refresh-secret",
                "expires_at": chrono::Local::now().timestamp_millis() + 60_000,
                "user": {"email": "legacy@example.com"}
            }))
            .unwrap(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        let session = load_session(&home).unwrap();
        assert!(
            !session.auto_refresh,
            "旧 session 缺少 remember/auto_refresh 时不得默认自动续期"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_dir_all(home);
    }
}
