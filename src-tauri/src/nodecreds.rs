//! 每账号节点凭证(星图 任务 A):数据层。
//!
//! 节点契约(已上线,勿改):
//! - POST {base}/issue-cred  body {"apiKey":"<2xapi Key>"}
//!   → 200 {"user","pass","quotaTotalBytes","quotaUsedBytes","proxyEndpoint"}
//!   → 401 {"error":"Key 无效或未充值"};403 {"error":"该账号本月已用满 10G"}
//! - 代理 http://<user>:<pass>@{base}(HTTP CONNECT),超配额 407。
//!
//! 职责:按 2xapi Key 换取/缓存每账号节点凭证;本地存储 `{codex_home}/accel-credentials.json`
//! v2 多账号形态,并兼容迁移旧单对象 `{user,pass}`(装入 legacy,供 custom 模式继续用)。
//!
//! 安全约定(仿 acclines):pass 永不进日志——NodeCred/Store 手写 Debug 脱敏 <redacted>;
//! 落盘原子写(临时文件 rename)+ 权限 0600。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;

/// 节点签发服务默认地址(生产签发只允许 HTTPS)。
pub const DEFAULT_ISSUE_BASE: &str = "https://156.238.251.207:443";
/// 节点代理地址仍由代理协议决定，不等同于签发接口地址。
pub const DEFAULT_PROXY_ENDPOINT: &str = "http://156.238.251.207:443";

/// 凭证文件名(与旧版单对象格式同路径,load 兼容迁移)。
const FILE_NAME: &str = "accel-credentials.json";

/// 单账号节点凭证。不派生 Debug:pass 经手写 Debug 脱敏(安全约定)。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeCred {
    pub user: String,
    pub pass: String,
    pub quota_total_bytes: u64,
    pub quota_used_bytes: u64,
    pub proxy_endpoint: String,
    pub issued_at: i64,
    pub degraded_to_direct: bool,
}

impl std::fmt::Debug for NodeCred {
    /// 手工 Debug:pass 脱敏为 <redacted>(安全约定,防日志泄露)。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeCred")
            .field("user", &self.user)
            .field("pass", &"<redacted>")
            .field("quota_total_bytes", &self.quota_total_bytes)
            .field("quota_used_bytes", &self.quota_used_bytes)
            .field("proxy_endpoint", &self.proxy_endpoint)
            .field("issued_at", &self.issued_at)
            .field("degraded_to_direct", &self.degraded_to_direct)
            .finish()
    }
}

/// v2 多账号凭证表:`creds` 按 key_hash(sha256 hex)索引;
/// `legacy`:旧单对象格式迁移而来的凭证(供 custom 模式用)。
/// version 字段必填(无 serde default)——旧 `{user,pass}` 文件缺 version,
/// 反序列化为 Store 必失败,从而落入 legacy 迁移分支。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Store {
    pub version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy: Option<crate::acclines::Cred>,
    #[serde(default)]
    pub creds: HashMap<String, NodeCred>,
}

impl std::fmt::Debug for Store {
    /// 手工 Debug:legacy 与表内凭证的 pass 一律脱敏(安全约定)。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("version", &self.version)
            .field("legacy", &self.legacy.as_ref().map(|_| "<redacted>"))
            .field("creds", &format!("{} entries", self.creds.len()))
            .finish()
    }
}

impl Default for Store {
    fn default() -> Self {
        Store::empty()
    }
}

impl Store {
    /// 空 v2 表(文件缺失/非法时的兜底)。
    pub fn empty() -> Store {
        Store {
            version: 2,
            legacy: None,
            creds: HashMap::new(),
        }
    }

    /// 按 2xapi Key 取该账号凭证(hash 索引)。
    pub fn get_for_key(&self, api_key: &str) -> Option<&NodeCred> {
        self.creds.get(&hash_key(api_key))
    }

    /// 写入/覆盖该账号凭证(以 key_hash 为索引)。
    pub fn set_for_key(&mut self, api_key: &str, cred: NodeCred) {
        self.creds.insert(hash_key(api_key), cred);
    }

    /// 旧单对象凭证(迁移而来),返回 {user,pass} 形态供 custom 模式用。
    pub fn legacy_cred(&self) -> Option<crate::acclines::Cred> {
        self.legacy.clone()
    }
}

// ── 文件存储(~/.codex/accel-credentials.json)──────────────

fn store_path(codex_home: &Path) -> std::path::PathBuf {
    codex_home.join(FILE_NAME)
}

/// 读取凭证表(兼容迁移):
/// - v2 结构 → 直接解析;
/// - 旧单对象 `{user,pass}` → 装入 legacy,视为 v2 空表;
/// - 文件缺失/非法 → 空 Store(legacy=None)。
pub fn load_store(codex_home: &Path) -> Store {
    let path = store_path(codex_home);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Store::empty();
    };
    if let Ok(s) = serde_json::from_str::<Store>(&raw) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        return s;
    }
    // 旧单对象格式 → 迁移为 v2(legacy 装载,creds 空)
    match serde_json::from_str::<crate::acclines::Cred>(&raw) {
        Ok(legacy) => {
            let store = Store {
                version: 2,
                legacy: Some(legacy),
                creds: HashMap::new(),
            };
            let _ = save_store(codex_home, &store);
            store
        }
        Err(_) => Store::empty(),
    }
}

/// 原子写凭证表:先写临时文件(权限 0600)再 rename;父目录缺失自动创建。
pub fn save_store(codex_home: &Path, store: &Store) -> std::io::Result<()> {
    std::fs::create_dir_all(codex_home)?;
    let data = serde_json::to_vec_pretty(store)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let path = store_path(codex_home);
    let tmp = codex_home.join(format!("{FILE_NAME}.tmp"));
    std::fs::write(&tmp, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, &path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

// ── 签发(POST {base}/issue-cred)──────────────────────────

/// 403(配额满)时的配额快照:尽力解析节点 error 文案与配额字段(节点可能不带数字)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaSnapshot {
    pub message: String,
    #[serde(default)]
    pub quota_total_bytes: Option<u64>,
    #[serde(default)]
    pub quota_used_bytes: Option<u64>,
}

/// 签发失败分类:配额满(带快照)/ Key 无效 / 网络不可达(含超时、非 JSON)。
#[derive(Debug, Clone, PartialEq)]
pub enum IssueErr {
    QuotaFull(Option<QuotaSnapshot>),
    KeyInvalid,
    Unreachable(String),
}

fn validate_issue_base(base_url: &str) -> Result<(), IssueErr> {
    let parsed = reqwest::Url::parse(base_url.trim_end_matches('/'))
        .map_err(|e| IssueErr::Unreachable(format!("签发地址无效: {e}")))?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")) => Ok(()),
        _ => Err(IssueErr::Unreachable(
            "拒绝向非回环 HTTP 地址发送签发请求,请配置 HTTPS".into(),
        )),
    }
}

/// 200 响应体(节点契约字段为 camelCase)。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssueResp {
    user: String,
    pass: String,
    #[serde(default)]
    quota_total_bytes: Option<u64>,
    #[serde(default)]
    quota_used_bytes: Option<u64>,
    #[serde(default)]
    proxy_endpoint: Option<String>,
}

/// 向节点签发该账号的代理凭证。
/// - 200 → Ok(NodeCred)(issued_at 取当前时间戳);
/// - 401 → KeyInvalid;403 → QuotaFull(尽力解析 body 里 error 文案);
/// - 其他/网络/超时/非 JSON → Unreachable。
pub async fn issue_node_cred(base_url: &str, api_key: &str) -> Result<NodeCred, IssueErr> {
    validate_issue_base(base_url)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .no_proxy() // 绕过系统代理,仿 probe.rs
        .build()
        .map_err(|e| IssueErr::Unreachable(e.to_string()))?;
    let url = format!("{}/issue-cred", base_url.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "apiKey": api_key }))
        .send()
        .await
        .map_err(|e| IssueErr::Unreachable(format!("签发请求失败: {e}")))?;

    match resp.status().as_u16() {
        200 => {
            let body: IssueResp = resp
                .json()
                .await
                .map_err(|e| IssueErr::Unreachable(format!("解析签发响应失败: {e}")))?;
            Ok(NodeCred {
                user: body.user,
                pass: body.pass,
                quota_total_bytes: body.quota_total_bytes.unwrap_or(0),
                quota_used_bytes: body.quota_used_bytes.unwrap_or(0),
                proxy_endpoint: body
                    .proxy_endpoint
                    .unwrap_or_else(|| DEFAULT_PROXY_ENDPOINT.to_string()),
                issued_at: chrono::Utc::now().timestamp(),
                degraded_to_direct: false,
            })
        }
        401 => Err(IssueErr::KeyInvalid),
        403 => {
            // 尽力解析快照:JSON 取 error 文案(及可能的配额字段);非 JSON 用原文兜底
            let raw = resp.text().await.unwrap_or_default();
            let snap = serde_json::from_str::<serde_json::Value>(&raw)
                .ok()
                .map(|v| {
                    let message = v
                        .get("error")
                        .and_then(|e| e.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| raw.trim().to_string());
                    QuotaSnapshot {
                        message,
                        quota_total_bytes: v.get("quotaTotalBytes").and_then(|x| x.as_u64()),
                        quota_used_bytes: v.get("quotaUsedBytes").and_then(|x| x.as_u64()),
                    }
                });
            Err(IssueErr::QuotaFull(snap))
        }
        other => Err(IssueErr::Unreachable(format!("节点返回 HTTP {other}"))),
    }
}

// ── 辅助:Key 哈希(sha256 hex)────────────────────────────

/// 2xapi Key → sha256 hex(本地多账号索引,避免明文 Key 落盘)。
pub fn hash_key(api_key: &str) -> String {
    let mut h = Sha256::new();
    h.update(api_key.as_bytes());
    hex::encode(h.finalize())
}

// ── 单测(全部 tempdir 沙箱,绝不触碰真实 ~/.codex)──────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    static N: AtomicU64 = AtomicU64::new(0);

    fn sandbox(label: &str) -> std::path::PathBuf {
        let n = N.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "2xapi-nodecreds-{label}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn cred(user: &str, used: u64) -> NodeCred {
        NodeCred {
            user: user.into(),
            pass: format!("pass-{user}"),
            quota_total_bytes: 10_737_418_240,
            quota_used_bytes: used,
            proxy_endpoint: "http://156.238.251.207:443".into(),
            issued_at: 1_760_000_000,
            degraded_to_direct: false,
        }
    }

    // ① 旧单对象文件 → 迁移为 v2 且 legacy 装载
    #[test]
    fn load_store_migrates_legacy_single_object() {
        let home = sandbox("legacy");
        std::fs::write(
            home.join(FILE_NAME),
            r#"{"user":"old-user","pass":"old-pass"}"#,
        )
        .unwrap();
        let s = load_store(&home);
        assert_eq!(s.version, 2);
        assert_eq!(s.creds.len(), 0, "旧文件迁移后 creds 应为空表");
        let lg = s.legacy_cred().expect("legacy 应装载");
        assert_eq!(
            (lg.user.as_str(), lg.pass.as_str()),
            ("old-user", "old-pass")
        );
        let persisted: Store =
            serde_json::from_str(&std::fs::read_to_string(home.join(FILE_NAME)).unwrap()).unwrap();
        assert_eq!(persisted.version, 2, "旧格式读取后应立即迁移落盘");
        assert!(persisted.legacy.is_some());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(home.join(FILE_NAME))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[test]
    fn loading_v2_repairs_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let home = sandbox("repair-mode");
        let path = home.join(FILE_NAME);
        std::fs::write(&path, serde_json::to_vec_pretty(&Store::empty()).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert_eq!(load_store(&home).version, 2);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    // ② v2 往返读写(多账号)
    #[test]
    fn store_v2_roundtrip_save_load() {
        let home = sandbox("roundtrip");
        let mut s = Store::empty();
        s.set_for_key("sk-key-a", cred("u-a", 100));
        s.set_for_key("sk-key-b", cred("u-b", 200));
        save_store(&home, &s).unwrap();
        let loaded = load_store(&home);
        assert_eq!(loaded.version, 2);
        assert!(loaded.legacy.is_none());
        assert_eq!(loaded.creds.len(), 2);
        assert_eq!(loaded.get_for_key("sk-key-a").unwrap().user, "u-a");
        assert_eq!(
            loaded.get_for_key("sk-key-b").unwrap().quota_used_bytes,
            200
        );
        assert!(
            loaded.get_for_key("sk-other").is_none(),
            "未签发账号 → None"
        );
        assert_eq!(
            loaded.get_for_key("sk-key-a").unwrap(),
            s.get_for_key("sk-key-a").unwrap()
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    // ③ 原子写后 load 一致 + 权限 0600 + 无 .tmp 残留
    #[test]
    fn save_store_atomic_and_permission_0600() {
        use std::os::unix::fs::PermissionsExt;
        let home = sandbox("atomic");
        let mut s = Store::empty();
        s.set_for_key("sk-x", cred("u-x", 1));
        save_store(&home, &s).unwrap();
        let meta = std::fs::metadata(home.join(FILE_NAME)).unwrap();
        assert_eq!(
            meta.permissions().mode() & 0o777,
            0o600,
            "凭证文件权限须 0600"
        );
        assert!(
            !home.join(format!("{FILE_NAME}.tmp")).exists(),
            "临时文件应已 rename 掉"
        );
        assert_eq!(load_store(&home), s, "写后读回应一致");
        // 文件缺失 → 空 Store
        let empty_home = sandbox("missing");
        assert_eq!(load_store(&empty_home), Store::empty());
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&empty_home);
    }

    // ④ issue_node_cred 三分支(mock 本地 tiny HTTP server)+ 不可达
    /// 可控 mock 节点:固定 status/body;记录收到的原始请求(供断言 body 含 apiKey)。
    async fn spawn_issue_server(
        status_line: &'static str,
        body: Arc<String>,
        seen: Arc<Mutex<Vec<String>>>,
    ) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let body = body.clone();
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let Ok(n) = sock.read(&mut buf).await else {
                        return;
                    };
                    seen.lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&buf[..n]).to_string());
                    let resp = format!(
                        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    fn issue_body() -> String {
        r#"{"user":"acc-1","pass":"sec-1","quotaTotalBytes":10737418240,"quotaUsedBytes":123,"proxyEndpoint":"http://156.238.251.207:443"}"#.into()
    }

    #[tokio::test]
    async fn issue_node_cred_ok_200() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let base = spawn_issue_server("200 OK", Arc::new(issue_body()), seen.clone()).await;
        let before = chrono::Utc::now().timestamp();
        let c = issue_node_cred(&base, "sk-abc").await.expect("200 应 Ok");
        assert_eq!((c.user.as_str(), c.pass.as_str()), ("acc-1", "sec-1"));
        assert_eq!(c.quota_total_bytes, 10_737_418_240);
        assert_eq!(c.quota_used_bytes, 123);
        assert_eq!(c.proxy_endpoint, "http://156.238.251.207:443");
        assert!(!c.degraded_to_direct);
        assert!(c.issued_at >= before, "issued_at 应为签发时刻");
        // 请求体应为 {"apiKey":...} 且打到 /issue-cred
        let req = seen.lock().unwrap()[0].clone();
        assert!(
            req.starts_with("POST /issue-cred "),
            "应 POST {base}/issue-cred"
        );
        assert!(req.contains(r#""apiKey":"sk-abc""#), "body 应含明文 apiKey");
    }

    #[tokio::test]
    async fn issue_node_cred_401_key_invalid() {
        let base = spawn_issue_server(
            "401 Unauthorized",
            Arc::new(r#"{"error":"Key 无效或未充值"}"#.into()),
            Arc::new(Mutex::new(Vec::new())),
        )
        .await;
        assert_eq!(
            issue_node_cred(&base, "sk-bad").await.unwrap_err(),
            IssueErr::KeyInvalid
        );
    }

    #[tokio::test]
    async fn issue_node_cred_403_quota_full_with_snapshot() {
        let base = spawn_issue_server(
            "403 Forbidden",
            Arc::new(r#"{"error":"该账号本月已用满 10G"}"#.into()),
            Arc::new(Mutex::new(Vec::new())),
        )
        .await;
        match issue_node_cred(&base, "sk-full").await.unwrap_err() {
            IssueErr::QuotaFull(Some(snap)) => {
                assert_eq!(snap.message, "该账号本月已用满 10G");
                assert_eq!(snap.quota_total_bytes, None, "契约 403 不带配额数字");
            }
            other => panic!("应为 QuotaFull(带快照),got {other:?}"),
        }
    }

    #[tokio::test]
    async fn issue_node_cred_unreachable() {
        // 127.0.0.1:9(discard 端口,本地必拒连)→ Unreachable
        match issue_node_cred("http://127.0.0.1:9", "sk-any")
            .await
            .unwrap_err()
        {
            IssueErr::Unreachable(_) => {}
            other => panic!("拒连应 Unreachable,got {other:?}"),
        }
    }

    #[test]
    fn production_issue_endpoint_uses_tls() {
        assert!(
            DEFAULT_ISSUE_BASE.starts_with("https://"),
            "生产签发端点必须使用 HTTPS"
        );
    }

    // ⑤ hash_key 稳定 + Debug 脱敏
    #[test]
    fn hash_key_stable_and_redacted_debug() {
        // sha256("abc") 标准向量,验证算法与编码
        assert_eq!(
            hash_key("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(hash_key("sk-1"), hash_key("sk-1"), "同 Key 稳定");
        assert_ne!(hash_key("sk-1"), hash_key("sk-2"), "异 Key 不同");
        assert_eq!(hash_key("sk-1").len(), 64, "hex 64 字符");
        // pass 永不进日志:Debug 输出须脱敏
        let c = cred("u", 0);
        let dbg = format!("{c:?}");
        assert!(
            dbg.contains("<redacted>") && !dbg.contains("pass-u"),
            "NodeCred Debug 须脱敏"
        );
        let mut s = Store::empty();
        s.legacy = Some(crate::acclines::Cred {
            user: "lu".into(),
            pass: "lp".into(),
        });
        s.set_for_key("k", cred("u2", 0));
        let sdbg = format!("{s:?}");
        assert!(
            sdbg.contains("<redacted>") && !sdbg.contains("lp"),
            "Store Debug 须脱敏"
        );
    }
}
