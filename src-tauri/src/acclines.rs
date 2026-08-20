//! 加速线路(阶段 4,开发任务书 §五):数据层 + 健康探测。
//!
//! 任务书要点:
//! - 线路表 AccLines(内置默认 + 远程拉取 + 本地缓存三源合并)
//! - scope 匹配 `match_line`:按供应商 base_url 域名匹配线路,命中走加速线
//! - 健康探测:每 30s 真实 GET 上游 /models 计时,连续 3 败摘除、1 成恢复
//! - 远程拉取:GET {service_url}/lines.json → ed25519 验签 → 写本地缓存;
//!   失败(网络/验签/服务端未就绪)→ 回退缓存 → 再回退内置表。
//!
//! 安全约定:凭证(accel-credentials.json)只从本地文件读取并注入运行内存;
//! 不进代码、不写日志、不落缓存文件(缓存落盘时凭证一律置 None);
//! AccLine 的 Debug 对凭证做脱敏(显示 <redacted>)。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 连续失败达到该阈值即摘除线路。
pub const FAIL_THRESHOLD: u32 = 3;

/// 线路凭证(user/pass)。不派生 Debug:防凭证经日志/调试输出泄露(安全约定)。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Cred {
    pub user: String,
    pub pass: String,
}

/// 单条加速线路。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct AccLine {
    pub id: String,
    pub name: String,
    pub endpoint: String, // 加速线上游 base_url(经线装配方用于 Proxy+basic auth)
    pub scope: Vec<String>, // 供应商 base_url 域名匹配条目(host 命中任一即走此线)
    pub priority: u32,    // 越小越优先
    pub enabled: bool,
    pub credential: Option<Cred>, // basic auth 凭证;内置线从 accel-credentials.json 注入
}

impl std::fmt::Debug for AccLine {
    /// 手工 Debug:凭证脱敏为 <redacted>(安全约定,防日志泄露)。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccLine")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("endpoint", &self.endpoint)
            .field("scope", &self.scope)
            .field("priority", &self.priority)
            .field("enabled", &self.enabled)
            .field(
                "credential",
                &self.credential.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// 线路表。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccLines {
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub lines: Vec<AccLine>,
}

// ── 内置默认表 + 凭证(安全约定:凭证不进代码)──────────────

/// 内置默认表:测试线路。credential 从 `{codex_home}/accel-credentials.json` 读取,否则 None。
fn builtin_default(codex_home: &Path) -> AccLines {
    AccLines {
        version: 1,
        lines: vec![AccLine {
            id: "test-1".into(),
            name: "测试线路".into(),
            endpoint: "http://156.238.251.207:443".into(),
            scope: vec!["*".into()], // 通用线:官方加速不限供应商(2026-08-16 用户定稿)
            priority: 1,
            enabled: true,
            credential: load_credentials(codex_home),
        }],
    }
}

/// 读取 `{codex_home}/accel-credentials.json` 的线路凭证(user/pass)。
/// 兼容两种形态:
/// - 旧 v1 单对象 `{user,pass}`(直接解析);
/// - nodecreds 写盘的 v2 Store `{version, legacy, creds}`(取 legacy;无 legacy 取表内任一凭证)。
///   双格式都解析失败时记录错误路径(不静默丢凭证);文件缺失/合法但无凭证 → None。
///   pub:供装配方(server.rs 的 test-node、gateway.rs 的 custom 线路)注入凭证。
pub fn load_credentials(codex_home: &Path) -> Option<Cred> {
    let path = codex_home.join("accel-credentials.json");
    let raw = std::fs::read_to_string(&path).ok()?;
    // v1 单对象 {user,pass}(v2 Store 文档无顶层 user/pass,此解析会失败,不误匹配)
    if let Ok(c) = serde_json::from_str::<Cred>(&raw) {
        return Some(c);
    }
    // v2 Store:legacy 优先,否则取表内任一账号凭证(都是可用的 basic auth 凭证)
    if let Ok(store) = serde_json::from_str::<crate::nodecreds::Store>(&raw) {
        if let Some(l) = store.legacy_cred() {
            return Some(l);
        }
        return store.creds.values().next().map(|c| Cred {
            user: c.user.clone(),
            pass: c.pass.clone(),
        });
    }
    eprintln!(
        "[acclines] load_credentials: 无法解析 {} (既非 v1 {{user,pass}},也非 v2 Store)",
        path.display()
    );
    None
}

/// 给没有凭证的线路注入本地 accel-credentials.json 的凭证(已有凭证的保留)。
fn attach_credentials(codex_home: &Path, lines: &mut AccLines) {
    let Some(cred) = load_credentials(codex_home) else {
        return;
    };
    for l in &mut lines.lines {
        if l.credential.is_none() {
            l.credential = Some(cred.clone());
        }
    }
}

// ── 本地缓存(acclines-cache.json)───────────────────────

fn cache_path(codex_home: &Path) -> PathBuf {
    codex_home.join("acclines-cache.json")
}

/// 写缓存:凭证一律置 None(安全约定:凭证只从 accel-credentials.json 注入)。
fn write_cache(codex_home: &Path, lines: &AccLines) {
    let mut clean = lines.clone();
    for l in &mut clean.lines {
        l.credential = None;
    }
    std::fs::create_dir_all(codex_home).ok();
    let _ = std::fs::write(
        cache_path(codex_home),
        serde_json::to_string_pretty(&clean).unwrap_or_default(),
    );
}

/// 读缓存并注入本地凭证;缺失/非法 → None。
fn load_cache(codex_home: &Path) -> Option<AccLines> {
    let raw = std::fs::read_to_string(cache_path(codex_home)).ok()?;
    let mut lines: AccLines = serde_json::from_str(&raw).ok()?;
    attach_credentials(codex_home, &mut lines);
    Some(lines)
}

/// 启动/离线加载:内置默认 + 本地缓存按 id 合并(缓存覆盖同名、新增追加)。
/// 供「铁匠·嫁接」装配:开机即得可用线路,再异步 fetch_lines 刷新。
pub fn load_lines(codex_home: &Path) -> AccLines {
    let mut merged = builtin_default(codex_home);
    if let Some(cached) = load_cache(codex_home) {
        for c in cached.lines {
            if let Some(b) = merged.lines.iter_mut().find(|b| b.id == c.id) {
                *b = c;
            } else {
                merged.lines.push(c);
            }
        }
        merged.version = merged.version.max(cached.version);
    }
    attach_credentials(codex_home, &mut merged);
    merged
}

// ── 远程拉取 + ed25519 验签(任务书 §五:启动拉 + 60min 刷新 + 本地缓存,失败用缓存)─

/// 远程线路表源配置(`{codex_home}/accel-remote.json`):服务端就绪后写入即生效,
/// 无需发版。缺失/字段空 = 服务端未就绪,刷新循环静默跳过(按内置/缓存表运行)。
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct RemoteSrc {
    #[serde(default)]
    pub service_url: String,
    #[serde(default)]
    pub pubkey_hex: String,
}

fn load_remote_src(codex_home: &Path) -> RemoteSrc {
    std::fs::read_to_string(codex_home.join("accel-remote.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// 远程拉取:GET `{service_url}/lines.json` → ed25519 验签 → 写本地缓存。
/// 失败(网络/验签/服务端未就绪)→ 回退本地缓存 → 再失败回退内置表。
/// 约定(服务端未就绪,本处定义契约):
///   - `pubkey_hex`:32 字节 ed25519 公钥的 hex(64 字符)
///   - 签名:对响应体原始字节签名的 64 字节 hex,放响应头 `X-Signature`
///
/// `service_url` 或 `pubkey_hex` 为空 → 视为服务端未就绪,直接走缓存/内置。
pub async fn fetch_lines(
    codex_home: &Path,
    service_url: &str,
    pubkey_hex: &str,
) -> Result<AccLines, String> {
    if !service_url.trim().is_empty() && !pubkey_hex.trim().is_empty() {
        if let Ok(mut lines) = fetch_remote(codex_home, service_url, pubkey_hex).await {
            attach_credentials(codex_home, &mut lines);
            return Ok(lines);
        }
    }
    if let Some(cached) = load_cache(codex_home) {
        return Ok(cached);
    }
    Ok(builtin_default(codex_home))
}

async fn fetch_remote(
    codex_home: &Path,
    service_url: &str,
    pubkey_hex: &str,
) -> Result<AccLines, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .no_proxy() // 绕过系统代理,仿 probe.rs
        .build()
        .map_err(|e| e.to_string())?;
    let url = format!("{}/lines.json", service_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("拉取失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("服务端未就绪(HTTP {})", resp.status()));
    }
    let sig_hex = resp
        .headers()
        .get("X-Signature")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| "响应缺 X-Signature".to_string())?
        .to_string();
    let body = resp
        .bytes()
        .await
        .map_err(|e| format!("读响应体失败: {e}"))?;
    verify_signature(pubkey_hex, &sig_hex, &body)?;
    let lines: AccLines =
        serde_json::from_slice(&body).map_err(|e| format!("解析线路表失败: {e}"))?;
    write_cache(codex_home, &lines);
    Ok(lines)
}

/// ed25519 验签(公钥/签名均为 hex;对原始 body 字节验签)。
fn verify_signature(pubkey_hex: &str, sig_hex: &str, body: &[u8]) -> Result<(), String> {
    let pk_bytes = hex::decode(pubkey_hex.trim()).map_err(|e| format!("pubkey hex 非法: {e}"))?;
    let pk_arr: [u8; 32] = pk_bytes
        .as_slice()
        .try_into()
        .map_err(|_| "pubkey 长度须为 32 字节".to_string())?;
    let key = ed25519_dalek::VerifyingKey::from_bytes(&pk_arr)
        .map_err(|e| format!("公钥解析失败: {e}"))?;
    let sig_bytes = hex::decode(sig_hex.trim()).map_err(|e| format!("签名 hex 非法: {e}"))?;
    let sig_arr: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| "签名长度须为 64 字节".to_string())?;
    let sig = ed25519_dalek::Signature::from_bytes(&sig_arr);
    key.verify_strict(body, &sig)
        .map_err(|_| "ed25519 验签失败".to_string())
}

// ── scope 匹配(任务书 §五:命中走加速线,未命中直连)──────────

/// 从 base_url 解析 host(去 http(s):// 前缀、去路径/查询、去端口),转小写。
fn host_of(base_url: &str) -> Option<String> {
    let s = base_url.trim();
    if s.is_empty() {
        return None;
    }
    // 去 scheme
    let after_scheme = match s.find("://") {
        Some(i) => &s[i + 3..],
        None => s,
    };
    // 去路径/查询/片段
    let end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let mut host = &after_scheme[..end];
    // 去端口(IPv6 字面量 [::1]:8080 特判;仅当 ':' 后全为数字才截断)
    if let Some(rest) = host.strip_prefix('[') {
        if let Some(close) = rest.find(']') {
            host = &rest[..close];
        }
    } else if let Some(i) = host.rfind(':') {
        let port = &host[i + 1..];
        if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) {
            host = &host[..i];
        }
    }
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// 单条 scope 匹配:host == scope、严格域后缀(api.2xa.cc.cd ⊃ 2xa.cc.cd)、
/// 或子串匹配(任务书 §五 允许,如 "evil2xa.cc.cd.evil.com" 命中 "2xa.cc.cd")。
fn host_matches(host: &str, scope: &str) -> bool {
    let s = scope.trim().trim_start_matches('.').to_ascii_lowercase();
    if s.is_empty() {
        return false;
    }
    if s == "*" {
        return true; // 通用线:不限供应商
    }
    host == s || host.ends_with(&format!(".{s}")) || host.contains(&s)
}

/// scope 匹配(纯函数):host 命中任一 line.scope 条目 → 返回 enabled 线中
/// priority 升序第一;无命中 None。
pub fn match_line<'a>(base_url: &str, lines: &'a [AccLine]) -> Option<&'a AccLine> {
    let host = host_of(base_url)?;
    let mut best: Option<&AccLine> = None;
    for line in lines {
        if !line.enabled {
            continue;
        }
        if !line.scope.iter().any(|s| host_matches(&host, s)) {
            continue;
        }
        match best {
            None => best = Some(line),
            Some(b) if line.priority < b.priority => best = Some(line),
            _ => {}
        }
    }
    best
}

/// 请求路径版匹配:命中线中按 priority 升序取第一条**未被摘除**的
/// (最佳线被摘除 → 次优顶上;全部被摘除 → None=直连)。
/// 展示类场景仍用 match_line(不受摘除影响)。
pub fn match_line_healthy<'a>(
    base_url: &str,
    lines: &'a [AccLine],
    health: &HealthState,
) -> Option<&'a AccLine> {
    let host = host_of(base_url)?;
    let mut candidates: Vec<&AccLine> = lines
        .iter()
        .filter(|l| l.enabled && l.scope.iter().any(|s| host_matches(&host, s)))
        .collect();
    // 稳定排序:同 priority 保表内先后(与 match_line 的「先出现者胜」一致)
    candidates.sort_by_key(|l| l.priority);
    candidates.into_iter().find(|l| health.is_available(&l.id))
}

// ── 健康探测(任务书 §五:每 30s,连续 3 败摘除、1 成恢复)────────

/// 单线健康记录。
#[derive(Debug, Clone, PartialEq)]
pub struct LineHealth {
    pub latency_ms: u64,
    pub fails: u32,
}

impl LineHealth {
    /// 是否已被摘除(连续失败 ≥ FAIL_THRESHOLD)。
    pub fn is_unhealthy(&self) -> bool {
        self.fails >= FAIL_THRESHOLD
    }
}

/// 健康状态表(仿 launcher 的 Arc<Mutex> 快照模式)。
/// `lines`:当前线路集(供健康循环快照探测;远程刷新后 set_lines 更新);
/// `table`:按 line_id 的健康记录。
#[derive(Default)]
pub struct HealthState {
    pub lines: Mutex<Vec<AccLine>>,
    pub table: Mutex<HashMap<String, LineHealth>>,
}

impl HealthState {
    pub fn new(lines: Vec<AccLine>) -> Self {
        HealthState {
            lines: Mutex::new(lines),
            table: Mutex::new(HashMap::new()),
        }
    }

    /// 整体替换线路集(远程 60min 刷新后调用)。
    pub fn set_lines(&self, lines: Vec<AccLine>) {
        *self.lines.lock().unwrap() = lines;
    }

    /// 线路是否在服务中(未被摘除)。无记录视为健康(尚未探测不误伤)。
    pub fn is_available(&self, line_id: &str) -> bool {
        self.table
            .lock()
            .unwrap()
            .get(line_id)
            .map(|h| !h.is_unhealthy())
            .unwrap_or(true)
    }

    /// 记录最近一次成功延迟(健康循环调用;apply_probe 只维护 fail/恢复计数)。
    fn record_latency(&self, line_id: &str, latency_ms: u64) {
        if let Some(e) = self.table.lock().unwrap().get_mut(line_id) {
            e.latency_ms = latency_ms;
        }
    }
}

/// 健康探测:真实 GET 上游 `{endpoint}/models` 计时(直连上游,非经加速线;
/// 经线计时由装配方做)。只有 2xx 响应算健康，401/407/5xx 等服务错误均进入失败计数。
pub async fn probe_line(client: &reqwest::Client, line: &AccLine) -> Result<u64, String> {
    let base = line.endpoint.trim_end_matches('/');
    let started = std::time::Instant::now();
    let resp = client
        .get(format!("{base}/models"))
        .send()
        .await
        .map_err(|e| format!("连接失败: {e}"))?;
    let latency_ms = started.elapsed().as_millis() as u64;
    if resp.status().is_success() {
        Ok(latency_ms)
    } else {
        Err(format!("服务端未就绪(HTTP {})", resp.status()))
    }
}

/// 探测用 HTTP 客户端(no_proxy + 10s 超时,仿 probe.rs:35-41)。
fn probe_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .no_proxy()
        .build()
        .unwrap_or_default()
}

/// 单次探测结果应用到健康表(纯判据,便于单测):
/// - ok → fails 归零(1 成恢复);
/// - !ok → fails +1,连续 FAIL_THRESHOLD 次 → 摘除(is_available=false / is_unhealthy=true)。
pub fn apply_probe(state: &HealthState, line_id: &str, ok: bool) {
    let mut m = state.table.lock().unwrap();
    let e = m.entry(line_id.to_string()).or_insert(LineHealth {
        latency_ms: 0,
        fails: 0,
    });
    e.fails = if ok { 0 } else { e.fails + 1 };
}

/// 后台健康探测循环:每 interval 快照 enabled 线路、并发探测、apply 到健康表。
/// 摘除后仍持续探测(探测成功即恢复,即「1 成恢复」)。interval 由装配方传(线上 30s;
/// 测试可传 200ms)。循环体抽为 `health_cycle` 便于测试直接驱动。
pub fn spawn_health_loop(state: Arc<HealthState>, interval: Duration) {
    tokio::spawn(async move {
        let client = probe_client();
        loop {
            health_cycle(&state, &client).await;
            tokio::time::sleep(interval).await;
        }
    });
}

/// 单轮探测+apply(循环体抽出,可测:测试可直接调用或用短 interval 的循环)。
async fn health_cycle(state: &HealthState, client: &reqwest::Client) {
    let snapshot: Vec<AccLine> = {
        let m = state.lines.lock().unwrap();
        m.iter().filter(|l| l.enabled).cloned().collect()
    };
    if snapshot.is_empty() {
        return;
    }
    let probes = futures_util::future::join_all(snapshot.iter().map(|l| {
        let client = client.clone();
        let l = l.clone();
        async move { (l.id.clone(), probe_line(&client, &l).await) }
    }))
    .await;
    for (id, res) in probes {
        match res {
            Ok(latency) => {
                apply_probe(state, &id, true);
                state.record_latency(&id, latency);
            }
            Err(_) => apply_probe(state, &id, false),
        }
    }
}

// ── 远程刷新循环(任务书 §五:启动即拉 + 每 60min 刷新;未配置静默跳过)──

/// 单轮刷新(循环体抽出,可测):读 accel-remote.json,未配置 → None(现有表不动);
/// 已配置 → fetch_lines(remote 失败内部回退缓存/内置)→ set_lines 整体替换。
async fn refresh_cycle(state: &HealthState, codex_home: &Path) -> Option<AccLines> {
    let src = load_remote_src(codex_home);
    if src.service_url.trim().is_empty() || src.pubkey_hex.trim().is_empty() {
        return None;
    }
    let lines = fetch_lines(codex_home, &src.service_url, &src.pubkey_hex)
        .await
        .ok()?;
    state.set_lines(lines.lines.clone());
    Some(lines)
}

/// 远程线路表刷新循环(interval 由装配方传,线上 60min;测试可短 interval 或直接
/// 驱动 refresh_cycle)。启动即拉一轮,再周期刷新。
pub fn spawn_refresh_loop(
    state: Arc<HealthState>,
    codex_home: std::path::PathBuf,
    interval: Duration,
) {
    tokio::spawn(async move {
        loop {
            refresh_cycle(&state, &codex_home).await;
            tokio::time::sleep(interval).await;
        }
    });
}

// ── 单测(任务书 §五)────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    fn sandbox(label: &str) -> PathBuf {
        let n = N.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("2xapi-acclines-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn line(id: &str, endpoint: &str, scope: &[&str], priority: u32, enabled: bool) -> AccLine {
        AccLine {
            id: id.into(),
            name: id.into(),
            endpoint: endpoint.into(),
            scope: scope.iter().map(|s| s.to_string()).collect(),
            priority,
            enabled,
            credential: None,
        }
    }

    // ── load_credentials:v1 单对象 与 nodecreds v2 Store 双格式兼容 ──

    #[test]
    fn load_credentials_v1_single_object() {
        let root = sandbox("cred-v1");
        std::fs::write(
            root.join("accel-credentials.json"),
            r#"{"user":"u1","pass":"p1"}"#,
        )
        .unwrap();
        let c = load_credentials(&root).expect("v1 应解析成功");
        assert_eq!((c.user.as_str(), c.pass.as_str()), ("u1", "p1"));
    }

    #[test]
    fn load_credentials_v2_store_legacy_and_entries() {
        // v2 Store:legacy 优先
        let root = sandbox("cred-v2-legacy");
        std::fs::write(
            root.join("accel-credentials.json"),
            r#"{"version":2,"legacy":{"user":"u2","pass":"p2"},"creds":{}}"#,
        )
        .unwrap();
        let c = load_credentials(&root).expect("v2 legacy 应解析成功");
        assert_eq!((c.user.as_str(), c.pass.as_str()), ("u2", "p2"));

        // v2 Store 无 legacy → 取表内任一账号凭证
        let root2 = sandbox("cred-v2-entry");
        std::fs::write(
            root2.join("accel-credentials.json"),
            r#"{"version":2,"creds":{"abc":{"user":"u3","pass":"p3","quota_total_bytes":0,"quota_used_bytes":0,"proxy_endpoint":"http://x","issued_at":0,"degraded_to_direct":false}}}"#,
        )
        .unwrap();
        let c2 = load_credentials(&root2).expect("v2 entries 应解析成功");
        assert_eq!((c2.user.as_str(), c2.pass.as_str()), ("u3", "p3"));
    }

    #[test]
    fn load_credentials_illegal_logs_and_returns_none() {
        // 非法 JSON(双格式均失败)→ None 且不 panic(错误路径 eprintln 已记录)
        let root = sandbox("cred-bad");
        std::fs::write(root.join("accel-credentials.json"), "{broken").unwrap();
        assert!(load_credentials(&root).is_none());
        // 缺失文件 → None
        assert!(load_credentials(&root).is_none());
    }

    // ── match_line:命中/不命中/多线按 priority/disabled 跳过 ──

    #[test]
    fn match_line_hits_suffix_and_returns_enabled_line() {
        let lines = vec![
            line("a", "http://x:1", &["2xa.cc.cd"], 1, true),
            line("b", "http://x:2", &["other.com"], 2, true),
        ];
        assert_eq!(
            match_line("https://api.2xa.cc.cd:443", &lines).unwrap().id,
            "a",
            "host 命中 scope"
        );
        assert!(
            match_line("https://openai.com", &lines).is_none(),
            "未命中 → None"
        );
        // scheme/端口/路径不影响 host 解析
        assert_eq!(
            match_line("http://api.2xa.cc.cd/v1/chat", &lines)
                .unwrap()
                .id,
            "a"
        );
    }

    #[test]
    fn wildcard_scope_matches_any_provider() {
        // 通用线(官方加速不限供应商,2026-08-16 用户定稿):scope="*" 对任意 base_url 命中
        let lines = vec![line("u1", "http://x:1", &["*"], 1, true)];
        for url in [
            "https://2xapi.cc.cd",
            "https://api.deepseek.example.com/v1",
            "https://opencode.ai/zen/go/v1",
        ] {
            assert_eq!(match_line(url, &lines).unwrap().id, "u1", "通配命中 {url}");
        }
        // 既有语义不受影响:具体域名 scope 仍按域匹配
        let scoped = vec![line("s1", "http://x:2", &["2xa.cc.cd"], 1, true)];
        assert!(
            match_line("https://other.example.com", &scoped).is_none(),
            "非通配仍不命中他域"
        );
    }

    #[test]
    fn match_line_picks_lowest_priority() {
        let lines = vec![
            line("low", "http://x:1", &["2xa.cc.cd"], 10, true),
            line("high", "http://x:2", &["2xa.cc.cd"], 1, true),
            line("mid", "http://x:3", &["2xa.cc.cd"], 5, true),
        ];
        assert_eq!(
            match_line("https://api.2xa.cc.cd", &lines).unwrap().id,
            "high"
        );
    }

    #[test]
    fn match_line_skips_disabled() {
        let lines = vec![
            line("off", "http://x:1", &["2xa.cc.cd"], 1, false),
            line("on", "http://x:2", &["2xa.cc.cd"], 2, true),
        ];
        assert_eq!(
            match_line("https://api.2xa.cc.cd", &lines).unwrap().id,
            "on"
        );
        let all_off = vec![line("off2", "http://x:3", &["2xa.cc.cd"], 1, false)];
        assert!(
            match_line("https://api.2xa.cc.cd", &all_off).is_none(),
            "全 disabled → None"
        );
    }

    #[test]
    fn match_line_exact_and_substring_and_dot_scope() {
        let lines = vec![line("a", "http://x:1", &["2xa.cc.cd"], 1, true)];
        assert_eq!(
            match_line("https://2xa.cc.cd", &lines).unwrap().id,
            "a",
            "完全相等"
        );
        assert_eq!(
            match_line("https://evil2xa.cc.cd.evil.com", &lines)
                .unwrap()
                .id,
            "a",
            "子串匹配"
        );
        // scope 带前导点也能匹配
        let dot = vec![line("b", "http://x:2", &[".2xa.cc.cd"], 1, true)];
        assert_eq!(match_line("https://api.2xa.cc.cd", &dot).unwrap().id, "b");
    }

    // ── apply_probe:连续 3 败摘除、1 成恢复 ──

    #[test]
    fn apply_probe_removes_after_three_fails_and_recovers_on_success() {
        let state = HealthState::new(vec![]);
        assert!(state.is_available("l1"), "未探测视为健康");
        apply_probe(&state, "l1", false);
        apply_probe(&state, "l1", false);
        assert!(state.is_available("l1"), "1、2 败仍可用");
        assert!(!state
            .table
            .lock()
            .unwrap()
            .get("l1")
            .unwrap()
            .is_unhealthy());
        apply_probe(&state, "l1", false);
        assert!(!state.is_available("l1"), "连续 3 败应摘除");
        assert!(state
            .table
            .lock()
            .unwrap()
            .get("l1")
            .unwrap()
            .is_unhealthy());
        assert_eq!(state.table.lock().unwrap().get("l1").unwrap().fails, 3);
        // 1 成 → 恢复
        apply_probe(&state, "l1", true);
        assert!(state.is_available("l1"), "1 成应恢复");
        assert_eq!(state.table.lock().unwrap().get("l1").unwrap().fails, 0);
    }

    #[test]
    fn apply_probe_one_success_resets_fails() {
        let state = HealthState::new(vec![]);
        apply_probe(&state, "l2", false);
        apply_probe(&state, "l2", false);
        apply_probe(&state, "l2", true);
        assert_eq!(
            state.table.lock().unwrap().get("l2").unwrap().fails,
            0,
            "中途一成就清零"
        );
        apply_probe(&state, "l2", false);
        assert_eq!(state.table.lock().unwrap().get("l2").unwrap().fails, 1);
    }

    // ── 内置表 + 凭证 + 缓存合并 ──

    #[test]
    fn builtin_default_has_test_line_and_credential_from_file() {
        let home = sandbox("builtin");
        std::fs::write(
            home.join("accel-credentials.json"),
            r#"{"user":"u","pass":"p"}"#,
        )
        .unwrap();
        let t = builtin_default(&home);
        assert_eq!(t.lines.len(), 1);
        assert_eq!(t.lines[0].id, "test-1");
        assert_eq!(t.lines[0].scope, vec!["*".to_string()]);
        assert_eq!(t.lines[0].endpoint, "http://156.238.251.207:443");
        assert_eq!(t.lines[0].credential.as_ref().unwrap().user, "u");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn load_lines_merges_cache_into_builtin() {
        let home = sandbox("merge");
        // 缓存:覆盖 test-1(改 endpoint)+ 新增 remote-x
        let cached = AccLines {
            version: 2,
            lines: vec![
                AccLine {
                    id: "test-1".into(),
                    name: "测试线路".into(),
                    endpoint: "http://new:1".into(),
                    scope: vec!["*".into()], // 通用线:官方加速不限供应商(2026-08-16 用户定稿)
                    priority: 1,
                    enabled: true,
                    credential: None,
                },
                line("remote-x", "http://r:1", &["api.cd"], 2, true),
            ],
        };
        std::fs::write(
            home.join("acclines-cache.json"),
            serde_json::to_string(&cached).unwrap(),
        )
        .unwrap();
        let merged = load_lines(&home);
        let ids: Vec<&str> = merged.lines.iter().map(|l| l.id.as_str()).collect();
        assert_eq!(ids, vec!["test-1", "remote-x"], "缓存覆盖同名、新增追加");
        assert_eq!(
            merged.lines[0].endpoint, "http://new:1",
            "缓存应覆盖内置 test-1"
        );
        assert_eq!(merged.version, 2);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn load_lines_without_cache_is_builtin_only() {
        let home = sandbox("no-cache");
        let t = load_lines(&home);
        assert_eq!(t.lines.len(), 1);
        assert_eq!(t.lines[0].id, "test-1");
        let _ = std::fs::remove_dir_all(&home);
    }

    // ── fetch_lines:远程成功写缓存 / 失败回退缓存 / 再回退内置 / 验签拒绝 ──

    /// 固定种子签名对,返回 (pubkey_hex, SigningKey)。
    fn signer() -> (String, ed25519_dalek::SigningKey) {
        let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let pub_hex = hex::encode(key.verifying_key().to_bytes());
        (pub_hex, key)
    }

    /// 本地签名线路服务器:固定 body + X-Signature 头。
    async fn spawn_signed_server(body: Arc<Vec<u8>>, sig_hex: Arc<String>) -> String {
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
                let sig_hex = sig_hex.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    if sock.read(&mut buf).await.is_err() {
                        return;
                    }
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nX-Signature: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        sig_hex, body.len(), String::from_utf8_lossy(body.as_slice())
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn fetch_lines_remote_success_writes_cache() {
        use ed25519_dalek::Signer;
        let home = sandbox("fetch-ok");
        let (pub_hex, key) = signer();
        let remote = AccLines {
            version: 3,
            lines: vec![line("remote-a", "http://r:1", &["a.cd"], 1, true)],
        };
        let body = Arc::new(serde_json::to_vec(&remote).unwrap());
        let sig = key.sign(body.as_slice());
        let sig_hex = Arc::new(hex::encode(sig.to_bytes()));
        let url = spawn_signed_server(body, sig_hex).await;

        let got = fetch_lines(&home, &url, &pub_hex).await.unwrap();
        assert_eq!(got.lines[0].id, "remote-a");
        // 缓存已写,且凭证被剥离(安全约定)
        let cached: AccLines = serde_json::from_str(
            &std::fs::read_to_string(home.join("acclines-cache.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(cached.lines[0].id, "remote-a");
        assert!(cached.lines[0].credential.is_none(), "缓存不应含凭证");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn match_line_healthy_skips_removed_and_falls_to_next() {
        let state = HealthState::new(vec![
            line("best", "http://a:1", &["api.up.com"], 1, true),
            line("second", "http://b:1", &["api.up.com"], 2, true),
        ]);
        let snap = state.lines.lock().unwrap().clone();
        // 全健康 → 最佳线
        assert_eq!(
            match_line_healthy("https://api.up.com/v1", &snap, &state)
                .unwrap()
                .id,
            "best"
        );
        // 最佳线 3 败摘除 → 次优顶上(match_line 不受影响仍指最佳)
        for _ in 0..FAIL_THRESHOLD {
            apply_probe(&state, "best", false);
        }
        assert_eq!(
            match_line_healthy("https://api.up.com/v1", &snap, &state)
                .unwrap()
                .id,
            "second"
        );
        assert_eq!(
            match_line("https://api.up.com/v1", &snap).unwrap().id,
            "best"
        );
        // 全部摘除 → None(直连)
        for _ in 0..FAIL_THRESHOLD {
            apply_probe(&state, "second", false);
        }
        assert!(match_line_healthy("https://api.up.com/v1", &snap, &state).is_none());
        // 1 成恢复 → 回服务(次优先)
        apply_probe(&state, "second", true);
        assert_eq!(
            match_line_healthy("https://api.up.com/v1", &snap, &state)
                .unwrap()
                .id,
            "second"
        );
        apply_probe(&state, "best", true);
        assert_eq!(
            match_line_healthy("https://api.up.com/v1", &snap, &state)
                .unwrap()
                .id,
            "best"
        );
    }

    #[tokio::test]
    async fn refresh_cycle_without_config_is_noop() {
        let home = sandbox("refresh-noop");
        let state = HealthState::new(vec![line("keep", "http://x:1", &["*"], 1, true)]);
        assert!(refresh_cycle(&state, &home).await.is_none());
        let ls = state.lines.lock().unwrap();
        assert_eq!(ls.len(), 1);
        assert_eq!(ls[0].id, "keep");
    }

    #[tokio::test]
    async fn refresh_cycle_replaces_lines_from_remote() {
        use ed25519_dalek::Signer;
        let home = sandbox("refresh-remote");
        let (pub_hex, key) = signer();
        let remote = AccLines {
            version: 7,
            lines: vec![line("r1", "http://r:1", &["api.remote.com"], 1, true)],
        };
        let body = Arc::new(serde_json::to_vec(&remote).unwrap());
        let sig = key.sign(body.as_slice());
        let sig_hex = Arc::new(hex::encode(sig.to_bytes()));
        let url = spawn_signed_server(body, sig_hex).await;
        std::fs::write(
            home.join("accel-remote.json"),
            serde_json::json!({ "service_url": url, "pubkey_hex": pub_hex }).to_string(),
        )
        .unwrap();

        let state = HealthState::new(vec![line("old", "http://old:1", &["*"], 1, true)]);
        assert!(refresh_cycle(&state, &home).await.is_some());
        {
            let ls = state.lines.lock().unwrap();
            assert_eq!(ls.len(), 1, "远程表整体替换(任务书 §五语义): {ls:?}");
            assert_eq!(ls[0].id, "r1");
        }
        assert!(home.join("acclines-cache.json").exists());
    }

    #[tokio::test]
    async fn fetch_lines_falls_back_to_cache_then_builtin() {
        let home = sandbox("fetch-fb");
        let cached = AccLines {
            version: 9,
            lines: vec![line("cached-1", "http://c:1", &["c.cd"], 1, true)],
        };
        std::fs::write(
            home.join("acclines-cache.json"),
            serde_json::to_string(&cached).unwrap(),
        )
        .unwrap();
        // 远程不可达(127.0.0.1:9 连接拒绝) → 回退缓存
        let got = fetch_lines(&home, "http://127.0.0.1:9", "00")
            .await
            .unwrap();
        assert_eq!(got.lines[0].id, "cached-1");
        // 删缓存 → 回退内置
        std::fs::remove_file(home.join("acclines-cache.json")).unwrap();
        let got2 = fetch_lines(&home, "http://127.0.0.1:9", "00")
            .await
            .unwrap();
        assert_eq!(got2.lines[0].id, "test-1");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn fetch_lines_rejects_tampered_signature() {
        use ed25519_dalek::Signer;
        let home = sandbox("fetch-tamper");
        let (pub_hex, _key) = signer();
        let body = Arc::new(
            serde_json::to_vec(&AccLines {
                version: 1,
                lines: vec![],
            })
            .unwrap(),
        );
        // 用错误私钥签名 → 验签必失败
        let wrong_key = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let sig = wrong_key.sign(body.as_slice());
        let sig_hex = Arc::new(hex::encode(sig.to_bytes()));
        let url = spawn_signed_server(body, sig_hex).await;

        let got = fetch_lines(&home, &url, &pub_hex).await.unwrap();
        assert_eq!(got.lines[0].id, "test-1", "验签失败应回退内置");
        assert!(
            !home.join("acclines-cache.json").exists(),
            "验签失败不应写缓存"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    // ── spawn_health_loop:200ms interval + 本地 mock 上游验证摘除/恢复流转 ──

    /// 可控 mock 上游:should_fail=true 时读请求后直接断连(连接层失败);false 时回 200。
    async fn spawn_mock(should_fail: Arc<AtomicBool>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let flag = should_fail.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    if sock.read(&mut buf).await.is_err() {
                        return;
                    }
                    if flag.load(Ordering::SeqCst) {
                        return; // 断连 → probe Err → fails++
                    }
                    let body = r#"{"data":[]}"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(), body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    async fn wait_until(mut cond: impl FnMut() -> bool, timeout_ms: u64) {
        let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
        while std::time::Instant::now() < deadline {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("wait_until 超时(条件未满足)");
    }

    #[tokio::test]
    async fn health_loop_removes_after_three_fails_and_recovers() {
        let flag = Arc::new(AtomicBool::new(true)); // 先断连
        let base = spawn_mock(flag.clone()).await;
        let state = Arc::new(HealthState::new(vec![line(
            "bad",
            &base,
            &["2xa.cc.cd"],
            1,
            true,
        )]));
        spawn_health_loop(state.clone(), Duration::from_millis(100));

        // 连续失败 → 摘除
        wait_until(|| !state.is_available("bad"), 5000).await;
        assert!(!state.is_available("bad"), "连续 3 败应被摘除");
        let fails = state.table.lock().unwrap().get("bad").unwrap().fails;
        assert!(fails >= 3, "fails 应 ≥3,got {fails}");

        // 上游恢复 → 1 成恢复
        flag.store(false, Ordering::SeqCst);
        wait_until(|| state.is_available("bad"), 5000).await;
        assert!(state.is_available("bad"), "探测成功后应恢复");
        assert_eq!(state.table.lock().unwrap().get("bad").unwrap().fails, 0);
    }
}
