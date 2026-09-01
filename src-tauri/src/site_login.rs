//! 内嵌官网登录(2026-09-02):腾讯国际验证码按「协议+域名+端口」校验 appId 白名单,
//! macOS WKWebView 的任何页面形态(127.0.0.1:8787 / tauri://localhost / https://localhost
//! 虚拟域)都被拒(1003),本机 80 端口又需管理员权限——内嵌验证码在 macOS 无解。
//! 改为弹独立窗口加载**真实官网登录页**(https://2xa.cc.cd/login):验证码在官网域名下
//! 原生运行(macOS/Windows 通吃);注入脚本轮询官网前端的 localStorage(sub2api 登录
//! 成功写 auth_token/refresh_token),拿到后经本机回环导航信标带回,拦截导航→取 token
//! →调 /api/v1/auth/me 补用户信息 → 落盘 2xapi-session.json → 主窗刷新。零服务端改动。

use tauri::{AppHandle, Manager, WebviewWindowBuilder};
use tauri::async_runtime as rt;

static APP_HANDLE: std::sync::OnceLock<AppHandle> = std::sync::OnceLock::new();

const SITE_LOGIN_URL: &str = "https://2xa.cc.cd/login";
const HANDOFF_PREFIX: &str = "http://127.0.0.1:8787/__site_login_handoff";

/// 注入到官网页面的脚本(WKUserScript 层,不受页面 CSP 限制):
/// 1) 顶部提示条,让用户确认这是官网登录;2) 轮询 token,到手即回传。
const INJECT_JS: &str = r#"
(function () {
  function mountBar() {
    try {
      var bar = document.createElement('div');
      bar.textContent = '🔒 2xapi 官网登录 · 完成后自动返回 Console';
      bar.style.cssText = 'position:fixed;top:0;left:0;right:0;z-index:2147483647;background:#0a7cff;color:#fff;font:12px -apple-system,sans-serif;padding:6px 10px;text-align:center;box-shadow:0 1px 6px rgba(0,0,0,.35)';
      var style = document.createElement('style');
      style.textContent = 'body{padding-top:30px !important}';
      (document.head || document.documentElement).appendChild(style);
      (document.body || document.documentElement).appendChild(bar);
    } catch (e) {}
  }
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', mountBar);
  } else { mountBar(); }

  var timer = setInterval(function () {
    try {
      var at = localStorage.getItem('auth_token');
      var rt = localStorage.getItem('refresh_token');
      if (!at || !rt) return;
      var host = location.hostname || '';
      if (host.indexOf('2xa.cc.cd') === -1 && host.indexOf('2xapi') === -1) return;
      clearInterval(timer);
      location.replace('http://127.0.0.1:8787/__site_login_handoff?at='
        + encodeURIComponent(at) + '&rt=' + encodeURIComponent(rt));
    } catch (e) {}
  }, 600);
})();
"#;

pub fn init(app: AppHandle) {
    let _ = APP_HANDLE.set(app);
}

/// 打开(或聚焦已开的)官网登录窗口。由 POST /api/auth/site-login 触发。
pub fn open() -> Result<(), String> {
    let app = APP_HANDLE.get().ok_or_else(|| "应用尚未就绪".to_string())?.clone();
    if let Some(w) = app.get_webview_window("site-login") {
        let _ = w.set_focus();
        return Ok(());
    }
    let nav_app = app.clone();
    WebviewWindowBuilder::new(
        &app,
        "site-login",
        tauri::WebviewUrl::External(SITE_LOGIN_URL.parse().unwrap()),
    )
    .title("2xapi 官网登录")
    .inner_size(460.0, 720.0)
    .min_inner_size(400.0, 560.0)
    .initialization_script(INJECT_JS)
    .on_navigation(move |url| {
        let s = url.as_str();
        if s.starts_with(HANDOFF_PREFIX) {
            let at = query_param(url, "at").unwrap_or_default();
            let rt_ = query_param(url, "rt").unwrap_or_default();
            if !at.is_empty() && !rt_.is_empty() {
                let handle = nav_app.clone();
                rt::spawn(async move {
                    if let Err(e) = complete(handle.clone(), at, rt_).await {
                        eprintln!("[site-login] 登录态回收失败: {e}");
                        if let Some(w) = handle.get_webview_window("site-login") {
                            let _ = w.eval(&format!(
                                "alert('登录信息带回失败: {}');location.replace('{}');",
                                e.replace('\'', ""),
                                SITE_LOGIN_URL
                            ));
                        }
                    }
                });
            }
            return false; // 信标不真正导航
        }
        // 顶层导航只放行站点自有域(验证码 iframe 不走此回调)
        let host = url.host_str().unwrap_or("");
        host.ends_with("2xa.cc.cd")
            || host == "2xapi.com"
            || host.ends_with("2xapi.cc.cd")
            || host == "2xapi.cn"
    })
    .build()
    .map_err(|e| format!("打开官网登录窗口失败: {e}"))?;
    Ok(())
}

fn query_param(url: &tauri::Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.to_string())
}

/// 拿到 token 后:补用户信息 → 落盘 session → 刷新主窗 → 关登录窗。
async fn complete(app: AppHandle, access_token: String, refresh_token: String) -> Result<(), String> {
    // expires_in 从 JWT exp 解;失败按 24h。
    let expires_in = jwt_exp_seconds(&access_token)
        .map(|exp| (exp - chrono::Local::now().timestamp()).max(60))
        .unwrap_or(24 * 3600);

    // 用新 token 拉 /auth/me 补 user(email/role/balance)。
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let mut user = serde_json::json!({});
    for base in ["https://2xa.cc.cd/api/v1", "https://2xapi.com/api/v1"] {
        if let Ok(resp) = client
            .get(format!("{base}/auth/me"))
            .bearer_auth(&access_token)
            .send()
            .await
        {
            if resp.status().is_success() {
                if let Ok(v) = resp.json::<serde_json::Value>().await {
                    user = v.get("data").cloned().unwrap_or(v);
                    break;
                }
            }
        }
    }

    let state = app.state::<crate::server::AppState>();
    let result = crate::auth::LoginResult {
        access_token,
        refresh_token,
        expires_in,
        user,
    };
    crate::auth::save_session(&state.codex_home, &result, false)?;

    if let Some(main) = app.get_webview_window("main") {
        let _ = main.eval("location.reload()");
    }
    if let Some(w) = app.get_webview_window("site-login") {
        let _ = w.close();
    }
    Ok(())
}

/// 解 JWT payload.exp(秒)。仅本机 2xapi 自签发 token,不做签名校验。
fn jwt_exp_seconds(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64url_decode(payload)?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("exp").and_then(|e| e.as_i64())
}

/// 极简 base64url(无填充)解码,免引入 base64 依赖。
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for &c in input.as_bytes() {
        if c == b'=' {
            break;
        }
        let val = TABLE.iter().position(|&t| t == c)? as u32;
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}
