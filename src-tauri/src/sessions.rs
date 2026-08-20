//! 历史会话管理(阶段 3,开发任务书 §四)。
//!
//! 读 `~/.codex/sqlite/codex-dev.db` 的 `local_thread_catalog`(真实 schema,探索笔记见交接日志):
//! 列表(updatedAt 倒序,分页,provider 过滤);repair(对账 rollout 文件与 db,补缺失/归属);
//! autoRepairBeforeHost 设置(host 前自动跑轻量 repair)。
//!
//! 安全约定:任何写操作(repair)前先整库备份到 backup_dir;只读操作永不改 db。
//! 本期只上「列表+修复+设置」,删除第二版(带备份恢复验证后再放)。

use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Stdio;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// 探测 Codex state DB：优先 `CODEX_HOME/state_5.sqlite`，再兼容旧 catalog DB。
fn state_db_path(codex_home: &Path) -> Option<PathBuf> {
    let candidates = [
        codex_home.join("state_5.sqlite"),
        codex_home.join("sqlite").join("state_5.sqlite"),
    ];
    candidates.into_iter().find(|path| path.is_file())
}

/// 探测历史 catalog DB；列表继续优先使用 catalog，repair 同时读取 state DB。
fn catalog_db_path(codex_home: &Path) -> Option<PathBuf> {
    let candidates = [
        codex_home.join("sqlite").join("codex-dev.db"),
        codex_home.join("sessions.sqlite"),
    ];
    candidates.into_iter().find(|path| path.is_file())
}

/// 探测真实 db 路径，保留旧函数供列表兼容。
#[allow(dead_code)] // 旧 catalog-only 列表兼容测试用；生产走 state 主列表。
fn probe_db_path(codex_home: &Path) -> Option<PathBuf> {
    catalog_db_path(codex_home).or_else(|| state_db_path(codex_home))
}

#[derive(Debug, Clone)]
struct RolloutInfo {
    path: PathBuf,
    cwd: String,
    title: String,
    provider: String,
    updated_at: i64,
    archived: bool,
}

#[derive(Debug, Clone, Default)]
struct CatalogInfo {
    title: String,
    cwd: String,
    provider: String,
    updated_at: i64,
    missing: bool,
}

#[derive(Debug, Clone)]
pub struct RepairJob {
    pub id: String,
    pub status: String,
    pub phase: String,
    pub processed: u64,
    pub total: u64,
    pub percent: u8,
    pub fixed: u64,
    pub backup_id: Option<String>,
    pub message: String,
    pub error: Option<String>,
}

#[derive(Default)]
struct JobStore {
    active: Option<String>,
    jobs: HashMap<String, RepairJob>,
}

#[derive(Debug, Clone)]
struct DeletePlan {
    ids: Vec<String>,
    created_at: i64,
}

static JOBS: LazyLock<Mutex<JobStore>> = LazyLock::new(|| Mutex::new(JobStore::default()));
static DELETE_PLANS: LazyLock<Mutex<HashMap<String, DeletePlan>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static HISTORY_WRITE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn valid_rollout_path(codex_home: &Path, raw: &str) -> Option<PathBuf> {
    let raw = raw.trim().trim_start_matches("\\?/");
    if raw.is_empty() {
        return None;
    }
    let path = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        codex_home.join(raw.trim_start_matches('/'))
    };
    let canonical = path.canonicalize().ok()?;
    let sessions = codex_home.join("sessions").canonicalize().ok();
    let archived = codex_home.join("archived_sessions").canonicalize().ok();
    let under = sessions
        .as_ref()
        .is_some_and(|root| canonical.starts_with(root))
        || archived
            .as_ref()
            .is_some_and(|root| canonical.starts_with(root));
    if !under || !canonical.is_file() || canonical.extension().is_none_or(|ext| ext != "jsonl") {
        return None;
    }
    Some(canonical)
}

fn state_rollouts(codex_home: &Path) -> HashMap<String, RolloutInfo> {
    let Some(path) = state_db_path(codex_home) else {
        return HashMap::new();
    };
    let Ok(conn) = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return HashMap::new();
    };
    let _ = conn.busy_timeout(std::time::Duration::from_secs(2));
    let mut out = HashMap::new();
    let updated_expr = if has_column(&conn, "threads", "updated_at_ms") {
        "COALESCE(updated_at_ms, updated_at * 1000)"
    } else {
        "updated_at * 1000"
    };
    let preview_expr = if has_column(&conn, "threads", "preview") {
        "preview"
    } else {
        "''"
    };
    let source_expr = if has_column(&conn, "threads", "thread_source") {
        "thread_source"
    } else {
        "NULL"
    };
    let sql = format!(
        "SELECT id, rollout_path, cwd, title, {preview_expr}, model_provider, {updated_expr}, archived, {source_expr} FROM threads"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return out;
    };
    let rows = stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let path: Option<String> = row.get(1)?;
        let cwd: String = row.get(2)?;
        let title: String = row.get(3).unwrap_or_default();
        let preview: String = row.get(4).unwrap_or_default();
        let provider: String = row.get(5).unwrap_or_default();
        let updated_at: i64 = row.get(6).unwrap_or(0);
        let archived: i64 = row.get(7).unwrap_or(0);
        let source: Option<String> = row.get(8).ok();
        Ok((
            id,
            path,
            cwd,
            if title.trim().is_empty() {
                preview
            } else {
                title
            },
            provider,
            updated_at,
            archived != 0,
            source,
        ))
    });
    let Ok(rows) = rows else {
        return out;
    };
    for row in rows.flatten() {
        let (id, raw_path, cwd, title, provider, updated_at, archived, source) = row;
        if source.as_deref() == Some("subagent") || source.as_deref() == Some("realtime_voice") {
            continue;
        }
        let Some(raw_path) = raw_path else { continue };
        if let Some(path) = valid_rollout_path(codex_home, &raw_path) {
            out.insert(
                id,
                RolloutInfo {
                    path,
                    cwd,
                    title,
                    provider,
                    updated_at,
                    archived,
                },
            );
        }
    }
    out
}

fn catalog_info(codex_home: &Path) -> HashMap<String, CatalogInfo> {
    let Some(path) = catalog_db_path(codex_home) else {
        return HashMap::new();
    };
    let Ok(conn) = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return HashMap::new();
    };
    let _ = conn.busy_timeout(Duration::from_secs(2));
    if !has_column(&conn, "local_thread_catalog", "thread_id") {
        return HashMap::new();
    }
    let sql = "SELECT thread_id, display_title, cwd, model_provider, CAST(source_updated_at * 1000 AS INTEGER), COALESCE(missing_candidate,0) FROM local_thread_catalog WHERE host_id='local' AND COALESCE(thread_source,'user') NOT IN ('subagent','realtime_voice')";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return HashMap::new();
    };
    let Ok(rows) = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            CatalogInfo {
                title: row.get::<_, String>(1).unwrap_or_default(),
                cwd: row.get::<_, String>(2).unwrap_or_default(),
                provider: row.get::<_, String>(3).unwrap_or_default(),
                updated_at: row.get::<_, i64>(4).unwrap_or_default(),
                missing: row.get::<_, i64>(5).unwrap_or_default() != 0,
            },
        ))
    }) else {
        return HashMap::new();
    };
    rows.flatten().collect()
}

#[derive(Debug, Clone)]
struct SessionSnapshot {
    codex_home: PathBuf,
    id: String,
    created_at: Instant,
    items: Vec<(String, RolloutInfo, bool)>,
}

static SESSION_SNAPSHOT: LazyLock<Mutex<HashMap<PathBuf, SessionSnapshot>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const SESSION_SNAPSHOT_TTL: Duration = Duration::from_secs(60);

fn invalidate_session_snapshot(codex_home: &Path) {
    if let Ok(mut snapshot) = SESSION_SNAPSHOT.lock() {
        snapshot.remove(codex_home);
    }
}

fn merge_session_infos(
    state: &HashMap<String, RolloutInfo>,
    catalog: &HashMap<String, CatalogInfo>,
) -> Vec<(String, RolloutInfo, bool)> {
    let mut items: Vec<_> = state
        .iter()
        .map(|(id, info)| {
            let mut info = info.clone();
            let missing = catalog.get(id).is_some_and(|entry| entry.missing);
            if let Some(entry) = catalog.get(id) {
                if !entry.title.trim().is_empty() {
                    info.title = entry.title.clone();
                }
                if info.cwd.trim().is_empty() && !entry.cwd.trim().is_empty() {
                    info.cwd = entry.cwd.clone();
                }
                if info.provider.trim().is_empty() && !entry.provider.trim().is_empty() {
                    info.provider = entry.provider.clone();
                }
                info.updated_at = info.updated_at.max(entry.updated_at);
            }
            (id.clone(), info, missing)
        })
        .collect();
    items.sort_by(|a, b| b.1.updated_at.cmp(&a.1.updated_at).then(b.0.cmp(&a.0)));
    items
}

fn user_session_infos(codex_home: &Path) -> Vec<(String, RolloutInfo, bool)> {
    merge_session_infos(&state_rollouts(codex_home), &catalog_info(codex_home))
}

fn page_from_items(
    codex_home: &Path,
    all: &[(String, RolloutInfo, bool)],
    page: usize,
    size: usize,
) -> Value {
    let total = all.len();
    let page = page.max(1);
    let size = size.clamp(1, 50);
    let start = page.saturating_sub(1).saturating_mul(size);
    let current = all.iter().skip(start).take(size);
    let active = current
        .clone()
        .filter(|(_, info, _)| !info.archived)
        .count();
    let archived = current.clone().filter(|(_, info, _)| info.archived).count();
    let items: Vec<_> = all
        .iter()
        .skip(start)
        .take(size)
        .map(|(id, info, missing)| {
            json!({
                "id": id,
                "title": if info.title.trim().is_empty() { "(无标题)" } else { &info.title },
                "cwd": info.cwd,
                "providerTag": if info.provider.trim().is_empty() { "unknown" } else { &info.provider },
                "updatedAt": info.updated_at,
                "archived": info.archived,
                "missing": missing,
                "resumable": !missing,
                "deletable": !missing,
            })
        })
        .collect();
    json!({
        "page": page,
        "size": size,
        "total": total,
        "hasMore": start.saturating_add(items.len()) < total,
        "pageStats": {"sessions": items.len(), "active": active, "archived": archived},
        "dbPaths": {
            "catalog": catalog_db_path(codex_home).map(|path| path.to_string_lossy().to_string()),
            "state": state_db_path(codex_home).map(|path| path.to_string_lossy().to_string()),
        },
        "items": items,
    })
}

/// Codex++ 风格会话页：state DB 为主数据源，catalog 仅补标题/缺失标记。
#[allow(dead_code)] // 兼容旧内部调用；生产 handler 使用 snapshot token。
pub fn list_sessions_page(codex_home: &Path, page: usize, size: usize) -> Value {
    page_from_items(codex_home, &user_session_infos(codex_home), page, size)
}

pub fn list_sessions_page_from_snapshot(
    codex_home: &Path,
    snapshot_id: &str,
    page: usize,
    size: usize,
) -> Result<Value, String> {
    let snapshot = SESSION_SNAPSHOT
        .lock()
        .map_err(|_| "会话快照锁已损坏".to_string())?;
    let Some(snapshot) = snapshot.get(codex_home) else {
        return Err("会话快照不存在或已过期".into());
    };
    if snapshot.id != snapshot_id
        || snapshot.codex_home != codex_home
        || snapshot.created_at.elapsed() > SESSION_SNAPSHOT_TTL
    {
        return Err("会话快照不存在或已过期".into());
    }
    Ok(page_from_items(codex_home, &snapshot.items, page, size))
}

fn codex_running() -> bool {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("pgrep")
            .args([
                "-f",
                "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT|codex .*app-server",
            ])
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

fn config_provider_sources(codex_home: &Path) -> (HashSet<String>, Option<String>) {
    let cfg = crate::config::read_toml(&codex_home.join("config.toml"));
    let mut providers = HashSet::new();
    if let Some(object) = cfg.get("model_providers").and_then(Value::as_object) {
        providers.extend(object.keys().cloned());
    }
    let current = cfg
        .get("model_provider")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    (providers, current)
}

fn inspect_from_maps(
    codex_home: &Path,
    state: &HashMap<String, RolloutInfo>,
    catalog: &HashMap<String, CatalogInfo>,
) -> Value {
    let (configured, current) = config_provider_sources(codex_home);
    let mut sources: HashMap<String, HashSet<&str>> = HashMap::new();
    sources.entry("custom".into()).or_default().insert("手动");
    sources.entry("openai".into()).or_default().insert("配置");
    for provider in configured {
        sources.entry(provider).or_default().insert("配置");
    }
    if let Some(provider) = current.as_deref() {
        sources.entry(provider.into()).or_default().insert("当前");
    }
    for info in state.values() {
        if !info.provider.trim().is_empty() {
            sources
                .entry(info.provider.clone())
                .or_default()
                .insert("会话");
        }
    }
    for info in catalog.values() {
        if !info.provider.trim().is_empty() {
            sources
                .entry(info.provider.clone())
                .or_default()
                .insert("索引");
        }
    }
    let index_path = codex_home.join("session_index.jsonl");
    if index_path.is_file() {
        for provider in state.values().map(|info| info.provider.clone()) {
            if !provider.trim().is_empty() {
                sources.entry(provider).or_default().insert("索引");
            }
        }
    }
    let mut targets: Vec<_> = sources
        .into_iter()
        .map(|(id, values)| {
            let order = ["配置", "会话", "索引", "手动", "当前"];
            let found: Vec<_> = order
                .into_iter()
                .filter(|source| values.contains(source))
                .collect();
            json!({
                "id": id,
                "label": format!("{}（{}）", id, found.join(" / ")),
                "sources": found,
                "isCurrent": current.as_deref() == Some(id.as_str()),
            })
        })
        .collect();
    targets.sort_by(|a, b| {
        a.get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .cmp(b.get("id").and_then(Value::as_str).unwrap_or(""))
    });
    let state_ids: HashSet<_> = state.keys().cloned().collect();
    let catalog_ids: HashSet<_> = catalog.keys().cloned().collect();
    let provider_mismatches = state
        .iter()
        .filter(|(id, info)| {
            catalog
                .get(*id)
                .is_some_and(|entry| entry.provider != info.provider)
        })
        .count();
    json!({
        "targets": targets,
        "counts": {
            "state": state.len(),
            "catalog": catalog.len(),
            "sessionIndex": count_nonempty_lines(&index_path),
            "active": state.values().filter(|info| !info.archived).count(),
            "archived": state.values().filter(|info| info.archived).count(),
        },
        "anomalies": {
            "missingCatalog": state_ids.difference(&catalog_ids).count(),
            "catalogOnly": catalog_ids.difference(&state_ids).count(),
            "missingRollouts": catalog.values().filter(|entry| entry.missing).count(),
            "providerMismatches": provider_mismatches,
        },
        "codexRunning": codex_running(),
    })
}

pub fn inspect_sessions(codex_home: &Path) -> Value {
    let state = state_rollouts(codex_home);
    let catalog = catalog_info(codex_home);
    let items = merge_session_infos(&state, &catalog);
    let id = uuid::Uuid::new_v4().to_string();
    let mut inspect = inspect_from_maps(codex_home, &state, &catalog);
    inspect["snapshotId"] = json!(id);
    if let Ok(mut snapshot) = SESSION_SNAPSHOT.lock() {
        snapshot.retain(|_, value| value.created_at.elapsed() <= SESSION_SNAPSHOT_TTL);
        snapshot.insert(
            codex_home.to_path_buf(),
            SessionSnapshot {
                codex_home: codex_home.to_path_buf(),
                id,
                created_at: Instant::now(),
                items,
            },
        );
    }
    inspect
}

fn count_nonempty_lines(path: &Path) -> usize {
    File::open(path)
        .ok()
        .map(|file| {
            BufReader::new(file)
                .lines()
                .map_while(Result::ok)
                .filter(|line| !line.trim().is_empty())
                .count()
        })
        .unwrap_or(0)
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|error| format!("读取备份校验文件失败: {error}"))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|error| format!("计算备份校验失败: {error}"))?;
    Ok(hex::encode(hasher.finalize()))
}

fn private_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|error| format!("创建历史备份目录失败: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("设置历史备份目录权限失败: {error}"))?;
    }
    Ok(())
}

fn private_file(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("设置历史备份文件权限失败: {error}"))?;
    }
    Ok(())
}

fn sqlite_snapshot(source: &Path, target: &Path) -> Result<(), String> {
    if target.exists() {
        std::fs::remove_file(target).map_err(|error| format!("清理旧快照失败: {error}"))?;
    }
    let conn =
        Connection::open(source).map_err(|error| format!("打开 SQLite 备份源失败: {error}"))?;
    conn.busy_timeout(Duration::from_secs(3))
        .map_err(|error| format!("设置 SQLite 等待超时失败: {error}"))?;
    let target = target
        .to_str()
        .ok_or_else(|| "SQLite 备份目标路径不是有效 UTF-8".to_string())?;
    conn.execute("VACUUM INTO ?1", [target])
        .map_err(|error| format!("创建 SQLite 一致性快照失败: {error}"))?;
    private_file(Path::new(target))
}

fn session_meta_record(path: &Path, expected_id: &str) -> Result<Value, String> {
    let file = File::open(path).map_err(|error| format!("读取 rollout 失败: {error}"))?;
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|error| format!("读取 rollout 行失败: {error}"))?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let payload = value.get("payload").and_then(Value::as_object);
        let id = payload
            .and_then(|payload| payload.get("id").or_else(|| payload.get("session_id")))
            .and_then(Value::as_str)
            .unwrap_or("");
        if id != expected_id {
            return Err("rollout 中的会话 ID 与请求不一致".into());
        }
        return Ok(json!({
            "path": path.to_string_lossy(),
            "line": index,
            "original": line,
        }));
    }
    Err("rollout 缺少 session_meta".into())
}

fn write_json_private(path: &Path, value: &Value) -> Result<(), String> {
    let raw =
        serde_json::to_vec_pretty(value).map_err(|error| format!("序列化历史备份失败: {error}"))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, raw).map_err(|error| format!("写历史备份临时文件失败: {error}"))?;
    private_file(&tmp)?;
    std::fs::rename(&tmp, path).map_err(|error| format!("保存历史备份失败: {error}"))?;
    private_file(path)
}

fn create_history_backup(
    codex_home: &Path,
    backup_dir: &Path,
    operation: &str,
    ids: &[String],
    target_provider: Option<&str>,
    include_rollouts: bool,
) -> Result<String, String> {
    private_dir(backup_dir)?;
    let id = format!(
        "history-{}-{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S%.3f"),
        uuid::Uuid::new_v4()
    );
    let root = backup_dir.join(&id);
    private_dir(&root)?;
    let mut files = Vec::new();
    if let Some(source) = state_db_path(codex_home) {
        let target = root.join("state.sqlite");
        sqlite_snapshot(&source, &target)?;
        files.push(json!({"kind":"state", "source":source, "backup":"state.sqlite", "sha256":sha256_file(&target)?}));
    }
    if let Some(source) = catalog_db_path(codex_home) {
        let target = root.join("catalog.sqlite");
        sqlite_snapshot(&source, &target)?;
        files.push(json!({"kind":"catalog", "source":source, "backup":"catalog.sqlite", "sha256":sha256_file(&target)?}));
    }
    let index = codex_home.join("session_index.jsonl");
    if index.is_file() {
        let target = root.join("session_index.jsonl");
        std::fs::copy(&index, &target)
            .map_err(|error| format!("备份 session_index 失败: {error}"))?;
        private_file(&target)?;
        files.push(json!({"kind":"session_index", "source":index, "backup":"session_index.jsonl", "sha256":sha256_file(&target)?}));
    }
    let state = state_rollouts(codex_home);
    let selected: Vec<String> = if ids.is_empty() {
        state.keys().cloned().collect()
    } else {
        ids.to_vec()
    };
    let mut meta = Vec::new();
    if include_rollouts {
        let rollout_dir = root.join("rollouts");
        private_dir(&rollout_dir)?;
        for thread_id in &selected {
            let info = state
                .get(thread_id)
                .ok_or_else(|| format!("找不到会话 {thread_id}"))?;
            let record = session_meta_record(&info.path, thread_id)?;
            let backup_name = format!("{thread_id}.jsonl");
            let target = rollout_dir.join(&backup_name);
            std::fs::copy(&info.path, &target)
                .map_err(|error| format!("备份 rollout 失败: {error}"))?;
            private_file(&target)?;
            files.push(json!({"kind":"rollout", "threadId":thread_id, "source":info.path, "backup":format!("rollouts/{backup_name}"), "sha256":sha256_file(&target)?}));
            meta.push(record);
        }
    } else {
        for thread_id in &selected {
            if let Some(info) = state.get(thread_id) {
                meta.push(session_meta_record(&info.path, thread_id)?);
            }
        }
    }
    write_json_private(&root.join("session-meta-backup.json"), &json!(meta))?;
    let manifest = json!({
        "version": 1,
        "kind": "history",
        "operation": operation,
        "codexHome": codex_home,
        "targetProvider": target_provider,
        "threadIds": selected,
        "createdAt": chrono::Utc::now().to_rfc3339(),
        "files": files,
    });
    write_json_private(&root.join("manifest.json"), &manifest)?;
    Ok(id)
}

fn history_backup_root(backup_dir: &Path, id: &str) -> Result<PathBuf, String> {
    if !id.starts_with("history-") || id.contains('/') || id.contains('\\') {
        return Err("历史备份 ID 非法".into());
    }
    let root = backup_dir.join(id);
    let canonical_base = backup_dir
        .canonicalize()
        .map_err(|_| "历史备份目录不存在".to_string())?;
    let canonical = root
        .canonicalize()
        .map_err(|_| "历史备份不存在".to_string())?;
    if !canonical.starts_with(canonical_base) || !canonical.is_dir() {
        return Err("历史备份路径越界".into());
    }
    Ok(canonical)
}

fn restore_db_snapshot(backup: &Path, target: &Path, expected_hash: &str) -> Result<(), String> {
    if sha256_file(backup)? != expected_hash {
        return Err("历史备份哈希校验失败".into());
    }
    let parent = target
        .parent()
        .ok_or_else(|| "数据库目标缺少父目录".to_string())?;
    std::fs::create_dir_all(parent).map_err(|error| format!("创建数据库目录失败: {error}"))?;
    let tmp = target.with_extension("2xapi-restore.tmp");
    std::fs::copy(backup, &tmp).map_err(|error| format!("恢复数据库临时文件失败: {error}"))?;
    std::fs::rename(&tmp, target).map_err(|error| format!("恢复数据库失败: {error}"))?;
    let _ = std::fs::remove_file(format!("{}-wal", target.display()));
    let _ = std::fs::remove_file(format!("{}-shm", target.display()));
    Ok(())
}

pub fn restore_history_backup(
    codex_home: &Path,
    backup_dir: &Path,
    id: &str,
) -> Result<Value, String> {
    let root = history_backup_root(backup_dir, id)?;
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("manifest.json"))
            .map_err(|error| format!("读取历史备份 manifest 失败: {error}"))?,
    )
    .map_err(|error| format!("历史备份 manifest 损坏: {error}"))?;
    if manifest.get("kind").and_then(Value::as_str) != Some("history") {
        return Err("备份不是历史会话备份".into());
    }
    for file in manifest
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let kind = file.get("kind").and_then(Value::as_str).unwrap_or("");
        let backup = file
            .get("backup")
            .and_then(Value::as_str)
            .ok_or_else(|| "备份项缺路径".to_string())?;
        let hash = file
            .get("sha256")
            .and_then(Value::as_str)
            .ok_or_else(|| "备份项缺哈希".to_string())?;
        let source = file
            .get("source")
            .and_then(Value::as_str)
            .ok_or_else(|| "备份项缺原路径".to_string())?;
        let backup_path = root.join(backup);
        match kind {
            "state" | "catalog" => restore_db_snapshot(&backup_path, Path::new(source), hash)?,
            "session_index" | "rollout" => {
                if sha256_file(&backup_path)? != hash {
                    return Err("历史备份哈希校验失败".into());
                }
                let target = PathBuf::from(source);
                if kind == "rollout" && valid_rollout_destination(codex_home, &target).is_none() {
                    return Err("rollout 恢复路径越界".into());
                }
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|error| format!("创建恢复目录失败: {error}"))?;
                }
                std::fs::copy(&backup_path, &target)
                    .map_err(|error| format!("恢复历史文件失败: {error}"))?;
            }
            _ => {}
        }
    }
    Ok(json!({"restored": true, "backupId": id}))
}

fn valid_rollout_destination(codex_home: &Path, target: &Path) -> Option<PathBuf> {
    let parent = target.parent()?.canonicalize().ok()?;
    let sessions = codex_home.join("sessions").canonicalize().ok();
    let archived = codex_home.join("archived_sessions").canonicalize().ok();
    let under = sessions
        .as_ref()
        .is_some_and(|root| parent.starts_with(root))
        || archived
            .as_ref()
            .is_some_and(|root| parent.starts_with(root));
    (under && target.extension().is_some_and(|ext| ext == "jsonl")).then(|| target.to_path_buf())
}

/// catalog 主表的列是否存在（新/旧 schema 自适应）。
fn has_column(db: &Connection, table: &str, column: &str) -> bool {
    let sql = format!("PRAGMA table_info({table})");
    if let Ok(mut stmt) = db.prepare(&sql) {
        if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(1)) {
            for col in rows.flatten() {
                if col == column {
                    return true;
                }
            }
        }
    }
    false
}

/// 单条会话(契约 items 项)。
#[allow(dead_code)] // 旧 catalog-only 契约测试保留；生产 handler 走 list_sessions_page。
#[derive(Debug, Clone)]
pub struct SessionItem {
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub provider_tag: String,
    pub updated_at_ms: i64,
    pub archived: bool,
    /// 对账缺失标记(repair 写 missing_candidate):API 输出供前端展示「缺失会话」用。
    pub missing: bool,
}

/// GET /api/sessions?page&size&provider → {total, items}
/// 按 updated_at 倒序;providerTag 从 catalog.model_provider(推不出标 "unknown")。
#[allow(dead_code)] // 旧 catalog-only 契约测试保留；生产 handler 走 list_sessions_page。
pub fn list_sessions(codex_home: &Path, page: usize, size: usize, provider: &str) -> Value {
    let Some(db_path) = probe_db_path(codex_home) else {
        return json!({ "total": 0, "items": [], "db": null });
    };
    let conn = match Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c,
        Err(_) => {
            return json!({ "total": 0, "items": [], "db": db_path.to_string_lossy(), "error": "打开数据库失败" })
        }
    };

    // 主表探测:新 schema local_thread_catalog / 旧 schema threads
    let catalog = "local_thread_catalog";
    let table = if has_column(&conn, catalog, "display_title") {
        catalog
    } else {
        "threads"
    };
    let cols = if table == catalog {
        // 新 schema:主键 (host_id, thread_id),无独立 id 列
        (
            "thread_id",
            "display_title",
            "cwd",
            "model_provider",
            "source_updated_at",
            "missing_candidate",
        )
    } else {
        (
            "id",
            "title",
            "cwd",
            "model_provider",
            "updated_at_ms",
            "archived",
        )
    };
    let archived_expr = if table == catalog {
        "0"
    } else {
        "COALESCE(archived,0)"
    };
    let missing_expr = if table == catalog {
        "COALESCE(missing_candidate,0)"
    } else {
        "0"
    };
    let updated_expr = if table == catalog {
        // source_updated_at 是 REAL 秒 → 毫秒
        "CAST(source_updated_at * 1000 AS INTEGER)"
    } else if has_column(&conn, table, "updated_at_ms") {
        "updated_at_ms"
    } else {
        "CAST(updated_at * 1000 AS INTEGER)"
    };

    let where_provider = if provider.is_empty() {
        String::new()
    } else {
        format!(" AND {} = :provider", cols.3)
    };

    // total
    let total_sql = format!("SELECT COUNT(*) FROM {table} WHERE 1=1{where_provider}");
    let total: i64 = if provider.is_empty() {
        conn.query_row(&total_sql, [], |r| r.get(0)).unwrap_or(0)
    } else {
        conn.query_row(&total_sql, rusqlite::params![provider], |r| r.get(0))
            .unwrap_or(0)
    };

    // 分页列表(updatedAt 倒序;同值按 id 倒序稳定)
    let page = page.max(1);
    let size = size.clamp(1, 100);
    let offset = (page - 1) * size;
    let list_sql = format!(
        "SELECT {col_id}, {col_title}, {col_cwd}, {col_provider}, {updated_expr}, {archived_expr}, {missing_expr}
         FROM {table}
         WHERE 1=1{where_provider}
         ORDER BY {updated_expr} DESC, {col_id} DESC
         LIMIT {size} OFFSET {offset}",
        col_id = cols.0, col_title = cols.1, col_cwd = cols.2, col_provider = cols.3,
        updated_expr = updated_expr, archived_expr = archived_expr, missing_expr = missing_expr,
    );

    let mut items = Vec::new();
    if let Ok(mut stmt) = conn.prepare(&list_sql) {
        let query = |r: &rusqlite::Row| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
            ))
        };
        let rows = if provider.is_empty() {
            stmt.query_map([], query)
        } else {
            stmt.query_map(rusqlite::params![provider], query)
        };
        if let Ok(rows) = rows {
            for row in rows.flatten() {
                let (id, title, cwd, provider_tag, updated_ms, archived, missing) = row;
                items.push(SessionItem {
                    id,
                    title: title.unwrap_or_default(),
                    cwd: cwd.unwrap_or_default(),
                    provider_tag: provider_tag
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "unknown".into()),
                    updated_at_ms: updated_ms,
                    archived: archived != 0,
                    missing: missing != 0,
                });
            }
        }
    }

    json!({
        "total": total,
        "items": items.iter().map(|s| json!({
            "id": s.id, "title": s.title, "cwd": s.cwd,
            "providerTag": s.provider_tag, "updatedAt": s.updated_at_ms, "archived": s.archived,
            "missing": s.missing,
        })).collect::<Vec<_>>(),
        "db": db_path.to_string_lossy(),
    })
}

// ── repair ─────────────────────────────────────────────────

fn update_job(id: &str, phase: &str, processed: u64, total: u64, percent: u8, message: &str) {
    if let Ok(mut store) = JOBS.lock() {
        if let Some(job) = store.jobs.get_mut(id) {
            job.phase = phase.into();
            job.processed = processed;
            job.total = total;
            job.percent = percent;
            job.message = message.into();
        }
    }
}

fn finish_job(id: &str, result: Result<(u64, String), String>) {
    if let Ok(mut store) = JOBS.lock() {
        if let Some(job) = store.jobs.get_mut(id) {
            match result {
                Ok((fixed, backup_id)) => {
                    job.status = "completed".into();
                    job.phase = "done".into();
                    job.percent = 100;
                    job.processed = job.total;
                    job.fixed = fixed;
                    job.backup_id = Some(backup_id);
                    job.message = format!("修复完成，共更新 {fixed} 项");
                }
                Err(error) => {
                    job.status = "failed".into();
                    job.phase = "failed".into();
                    job.error = Some(error.clone());
                    job.message = error;
                }
            }
        }
        if store.active.as_deref() == Some(id) {
            store.active = None;
        }
    }
}

fn replace_session_meta_provider(
    path: &Path,
    thread_id: &str,
    target: &str,
) -> Result<bool, String> {
    let file = File::open(path).map_err(|error| format!("读取 rollout 失败: {error}"))?;
    let mut lines = Vec::new();
    let mut changed = false;
    let mut found = false;
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|error| format!("读取 rollout 行失败: {error}"))?;
        let mut output = line.clone();
        if let Ok(mut value) = serde_json::from_str::<Value>(&line) {
            if value.get("type").and_then(Value::as_str) == Some("session_meta") {
                let payload = value.get_mut("payload").and_then(Value::as_object_mut);
                let id = payload
                    .as_ref()
                    .and_then(|payload| payload.get("id").or_else(|| payload.get("session_id")))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if id != thread_id {
                    return Err("rollout 中的会话 ID 与目标不一致".into());
                }
                found = true;
                if let Some(payload) = payload {
                    if payload.get("model_provider").and_then(Value::as_str) != Some(target) {
                        payload.insert("model_provider".into(), json!(target));
                        output = serde_json::to_string(&value)
                            .map_err(|error| format!("编码 session_meta 失败: {error}"))?;
                        changed = true;
                    }
                }
            }
        }
        lines.push(output);
    }
    if !found {
        return Err("rollout 缺少 session_meta".into());
    }
    if !changed {
        return Ok(false);
    }
    let tmp = path.with_extension("jsonl.2xapi-sync.tmp");
    let mut writer = BufWriter::new(
        File::create(&tmp).map_err(|error| format!("创建 rollout 临时文件失败: {error}"))?,
    );
    for line in lines {
        writeln!(writer, "{line}").map_err(|error| format!("写 rollout 临时文件失败: {error}"))?;
    }
    writer
        .flush()
        .map_err(|error| format!("刷新 rollout 临时文件失败: {error}"))?;
    std::fs::rename(&tmp, path).map_err(|error| format!("替换 rollout 失败: {error}"))?;
    Ok(true)
}

fn sync_provider(codex_home: &Path, target: &str, job_id: &str) -> Result<u64, String> {
    let sessions = user_session_infos(codex_home);
    let total = sessions.len() as u64;
    let mut fixed = 0u64;
    update_job(job_id, "rollout", 0, total, 20, "扫描历史会话");
    for (index, (thread_id, info, _)) in sessions.iter().enumerate() {
        if replace_session_meta_provider(&info.path, thread_id, target)? {
            fixed += 1;
        }
        if index % 10 == 0 || index + 1 == sessions.len() {
            let percent = 20 + (((index + 1) as f64 / sessions.len().max(1) as f64) * 35.0) as u8;
            update_job(
                job_id,
                "rollout",
                (index + 1) as u64,
                total,
                percent,
                "同步会话文件",
            );
        }
    }
    if let Some(path) = state_db_path(codex_home) {
        let conn =
            Connection::open(path).map_err(|error| format!("打开 state DB 失败: {error}"))?;
        conn.busy_timeout(Duration::from_secs(3))
            .map_err(|error| error.to_string())?;
        fixed += conn
            .execute(
                "UPDATE threads SET model_provider=?1 WHERE COALESCE(model_provider,'')<>?1 AND COALESCE(thread_source,'user') NOT IN ('subagent','realtime_voice')",
                [target],
            )
            .map_err(|error| format!("同步 state provider 失败: {error}"))? as u64;
    }
    update_job(job_id, "state", total, total, 68, "同步 state 数据库");
    if let Some(path) = catalog_db_path(codex_home) {
        let conn =
            Connection::open(path).map_err(|error| format!("打开 catalog DB 失败: {error}"))?;
        conn.busy_timeout(Duration::from_secs(3))
            .map_err(|error| error.to_string())?;
        fixed += conn
            .execute(
                "UPDATE local_thread_catalog SET model_provider=?1 WHERE host_id='local' AND COALESCE(model_provider,'')<>?1 AND COALESCE(thread_source,'user') NOT IN ('subagent','realtime_voice')",
                [target],
            )
            .map_err(|error| format!("同步 catalog provider 失败: {error}"))? as u64;
        let _ = conn.execute(
            "UPDATE local_thread_catalog_metadata SET catalog_revision=catalog_revision+1 WHERE id=1",
            [],
        );
    }
    update_job(job_id, "catalog", total, total, 83, "同步 catalog 索引");
    let repair = repair_sessions(codex_home, &codex_home.join("config-backups"));
    if let Some(error) = repair.get("error").and_then(Value::as_str) {
        return Err(error.into());
    }
    fixed += repair.get("fixed").and_then(Value::as_u64).unwrap_or(0);
    let removed_ghosts = prune_ghost_session_index(codex_home)?;
    fixed += removed_ghosts;
    update_job(job_id, "verify", total, total, 95, "校验数据库完整性");
    for path in [state_db_path(codex_home), catalog_db_path(codex_home)]
        .into_iter()
        .flatten()
    {
        let conn = Connection::open(path).map_err(|error| error.to_string())?;
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .map_err(|error| error.to_string())?;
        if integrity != "ok" {
            return Err(format!("SQLite 完整性校验失败: {integrity}"));
        }
    }
    Ok(fixed)
}

fn collect_reference_ids(codex_home: &Path) -> HashSet<String> {
    let mut ids = HashSet::new();
    let mut add_token = |raw: &str| {
        let value = raw.trim();
        if !value.is_empty() {
            ids.insert(value.to_string());
        }
        for start in 0..value.len().saturating_sub(35) {
            let end = start + 36;
            if value.is_char_boundary(start)
                && value.is_char_boundary(end)
                && uuid::Uuid::parse_str(&value[start..end]).is_ok()
            {
                ids.insert(value[start..end].to_string());
            }
        }
    };
    let mut stack = vec![
        codex_home.join("sessions"),
        codex_home.join("archived_sessions"),
    ];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                add_token(name);
            }
            let Ok(file) = File::open(&path) else {
                continue;
            };
            for line in BufReader::new(file).lines().map_while(Result::ok) {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if value.get("type").and_then(Value::as_str) == Some("session_meta") {
                    if let Some(payload) = value.get("payload").and_then(Value::as_object) {
                        if let Some(id) = payload
                            .get("id")
                            .or_else(|| payload.get("session_id"))
                            .and_then(Value::as_str)
                        {
                            add_token(id);
                        }
                    }
                }
            }
        }
    }

    let tables = [
        ("threads", &["id"][..]),
        ("local_thread_catalog", &["thread_id"][..]),
        ("automation_runs", &["thread_id", "id"][..]),
        ("inbox_items", &["thread_id", "id"][..]),
        ("sessions", &["thread_id", "id"][..]),
        ("messages", &["thread_id", "session_id"][..]),
        ("thread_dynamic_tools", &["thread_id"][..]),
        ("thread_goals", &["thread_id"][..]),
        (
            "thread_spawn_edges",
            &["parent_thread_id", "child_thread_id"][..],
        ),
        ("stage1_outputs", &["thread_id", "session_id"][..]),
        ("agent_job_items", &["thread_id", "session_id"][..]),
    ];
    for db_path in [state_db_path(codex_home), catalog_db_path(codex_home)]
        .into_iter()
        .flatten()
    {
        let Ok(conn) = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        else {
            continue;
        };
        for (table, columns) in tables {
            for column in columns {
                if !has_column(&conn, table, column) {
                    continue;
                }
                let sql = format!("SELECT {column} FROM {table}");
                let Ok(mut stmt) = conn.prepare(&sql) else {
                    continue;
                };
                let Ok(rows) = stmt.query_map([], |row| row.get::<_, Option<String>>(0)) else {
                    continue;
                };
                for value in rows.flatten().flatten() {
                    add_token(&value);
                }
            }
        }
    }
    ids
}

fn strict_index_ghost_id(line: &[u8], references: &HashSet<String>) -> Option<String> {
    let text = std::str::from_utf8(line)
        .ok()?
        .trim_end_matches(['\r', '\n']);
    let value = serde_json::from_str::<Value>(text).ok()?;
    let object = value.as_object()?;
    if object.len() != 3
        || !object.contains_key("id")
        || !object.contains_key("thread_name")
        || !object.contains_key("updated_at")
    {
        return None;
    }
    let id = object.get("id")?.as_str()?.trim();
    if id.is_empty()
        || object.get("thread_name")?.as_str()?.trim().is_empty()
        || object.get("updated_at")?.as_str()?.trim().is_empty()
        || references.contains(id)
    {
        return None;
    }
    Some(id.to_string())
}

fn prune_ghost_session_index(codex_home: &Path) -> Result<u64, String> {
    let path = codex_home.join("session_index.jsonl");
    if !path.is_file() {
        return Ok(0);
    }
    let mut raw = Vec::new();
    File::open(&path)
        .map_err(|error| format!("读取 session_index 失败: {error}"))?
        .read_to_end(&mut raw)
        .map_err(|error| format!("读取 session_index 失败: {error}"))?;
    let references = collect_reference_ids(codex_home);
    let mut kept = Vec::with_capacity(raw.len());
    let mut removed = 0u64;
    for segment in raw.split_inclusive(|byte| *byte == b'\n') {
        let content = segment.strip_suffix(b"\n").unwrap_or(segment);
        if strict_index_ghost_id(content, &references).is_some() {
            removed += 1;
        } else {
            kept.extend_from_slice(segment);
        }
    }
    if removed == 0 {
        return Ok(0);
    }
    let tmp = path.with_extension(format!("jsonl.2xapi-ghost-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = File::create(&tmp)
            .map_err(|error| format!("创建 session_index 临时文件失败: {error}"))?;
        file.write_all(&kept)
            .map_err(|error| format!("写 session_index 临时文件失败: {error}"))?;
        file.flush()
            .map_err(|error| format!("刷新 session_index 临时文件失败: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("同步 session_index 临时文件失败: {error}"))?;
        private_file(&tmp)?;
        std::fs::rename(&tmp, &path)
            .map_err(|error| format!("替换 session_index 失败: {error}"))?;
        Ok(removed)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

pub fn start_repair_job(
    codex_home: PathBuf,
    backup_dir: PathBuf,
    target_provider: String,
) -> Result<String, String> {
    if target_provider.trim().is_empty()
        || !target_provider
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    {
        return Err("同步目标 provider 非法".into());
    }
    if codex_running() {
        return Err("Codex 正在运行，请先点击“重新打开 Codex”完成退出后再修复".into());
    }
    invalidate_session_snapshot(&codex_home);
    let id = uuid::Uuid::new_v4().to_string();
    {
        let mut store = JOBS.lock().map_err(|_| "修复任务锁已损坏".to_string())?;
        if store.active.is_some() {
            return Err("已有历史会话修复任务正在运行".into());
        }
        store.active = Some(id.clone());
        store.jobs.insert(
            id.clone(),
            RepairJob {
                id: id.clone(),
                status: "running".into(),
                phase: "inspect".into(),
                processed: 0,
                total: 0,
                percent: 5,
                fixed: 0,
                backup_id: None,
                message: "正在检查历史会话".into(),
                error: None,
            },
        );
    }
    let job_id = id.clone();
    std::thread::spawn(move || {
        let result = (|| {
            let _guard = HISTORY_WRITE_LOCK
                .lock()
                .map_err(|_| "历史写入锁损坏".to_string())?;
            update_job(&job_id, "backup", 0, 0, 15, "创建修复前完整备份");
            let backup_id = create_history_backup(
                &codex_home,
                &backup_dir,
                "repair",
                &[],
                Some(&target_provider),
                true,
            )?;
            update_job(&job_id, "backup", 0, 0, 20, "备份完成");
            match sync_provider(&codex_home, &target_provider, &job_id) {
                Ok(fixed) => Ok((fixed, backup_id)),
                Err(error) => {
                    let _ = restore_history_backup(&codex_home, &backup_dir, &backup_id);
                    Err(error)
                }
            }
        })();
        finish_job(&job_id, result);
    });
    Ok(id)
}

pub fn get_repair_job(id: &str) -> Option<Value> {
    let store = JOBS.lock().ok()?;
    let job = store.jobs.get(id)?;
    Some(json!({
        "id": job.id,
        "status": job.status,
        "phase": job.phase,
        "processed": job.processed,
        "total": job.total,
        "percent": job.percent,
        "fixed": job.fixed,
        "backupId": job.backup_id,
        "message": job.message,
        "error": job.error,
    }))
}

pub fn preview_delete(codex_home: &Path, ids: &[String]) -> Result<Value, String> {
    if ids.is_empty() || ids.len() > 50 {
        return Err("请选择 1–50 个会话".into());
    }
    let state = state_rollouts(codex_home);
    let mut unique = HashSet::new();
    let mut items = Vec::new();
    for raw in ids {
        let id = uuid::Uuid::parse_str(raw.trim())
            .map_err(|_| format!("会话 ID 无效: {raw}"))?
            .to_string();
        if !unique.insert(id.clone()) {
            continue;
        }
        let info = state
            .get(&id)
            .ok_or_else(|| format!("会话不存在或不可删除: {id}"))?;
        session_meta_record(&info.path, &id)?;
        items.push(json!({"id":id,"title":info.title,"archived":info.archived,"cwd":info.cwd}));
    }
    let token = uuid::Uuid::new_v4().to_string();
    DELETE_PLANS
        .lock()
        .map_err(|_| "删除预览锁已损坏".to_string())?
        .insert(
            token.clone(),
            DeletePlan {
                ids: unique.into_iter().collect(),
                created_at: chrono::Utc::now().timestamp(),
            },
        );
    Ok(json!({
        "confirmToken": token,
        "count": items.len(),
        "items": items,
        "codexRunning": codex_running(),
        "warnings": if codex_running() { vec!["Codex 正在运行，请先退出后再删除"] } else { Vec::<&str>::new() },
    }))
}

fn delete_from_databases(codex_home: &Path, ids: &[String]) -> Result<u64, String> {
    let mut deleted = 0u64;
    if let Some(path) = state_db_path(codex_home) {
        let mut conn =
            Connection::open(path).map_err(|error| format!("打开 state DB 失败: {error}"))?;
        conn.busy_timeout(Duration::from_secs(3))
            .map_err(|error| error.to_string())?;
        let tx = conn
            .transaction()
            .map_err(|error| format!("开启 state 删除事务失败: {error}"))?;
        for id in ids {
            if has_column(&tx, "thread_dynamic_tools", "thread_id") {
                tx.execute("DELETE FROM thread_dynamic_tools WHERE thread_id=?1", [id])
                    .map_err(|error| format!("清理动态工具失败: {error}"))?;
            }
            if has_column(&tx, "thread_spawn_edges", "child_thread_id") {
                tx.execute(
                    "DELETE FROM thread_spawn_edges WHERE child_thread_id=?1 OR parent_thread_id=?1",
                    [id],
                )
                .map_err(|error| format!("清理线程关系失败: {error}"))?;
            }
            deleted += tx
                .execute("DELETE FROM threads WHERE id=?1", [id])
                .map_err(|error| format!("删除 state 会话失败: {error}"))?
                as u64;
        }
        tx.commit()
            .map_err(|error| format!("提交 state 删除失败: {error}"))?;
    }
    if let Some(path) = catalog_db_path(codex_home) {
        let mut conn =
            Connection::open(path).map_err(|error| format!("打开 catalog DB 失败: {error}"))?;
        conn.busy_timeout(Duration::from_secs(3))
            .map_err(|error| error.to_string())?;
        let tx = conn
            .transaction()
            .map_err(|error| format!("开启 catalog 删除事务失败: {error}"))?;
        for id in ids {
            tx.execute(
                "DELETE FROM local_thread_catalog WHERE host_id='local' AND thread_id=?1",
                [id],
            )
            .map_err(|error| format!("删除 catalog 会话失败: {error}"))?;
        }
        let _ = tx.execute(
            "UPDATE local_thread_catalog_metadata SET catalog_revision=catalog_revision+1 WHERE id=1",
            [],
        );
        tx.commit()
            .map_err(|error| format!("提交 catalog 删除失败: {error}"))?;
    }
    Ok(deleted)
}

fn prune_session_index(codex_home: &Path, deleted: &HashSet<String>) -> Result<(), String> {
    let path = codex_home.join("session_index.jsonl");
    if !path.is_file() {
        return Ok(());
    }
    let file = File::open(&path).map_err(|error| format!("读取 session_index 失败: {error}"))?;
    let mut kept = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|error| format!("读取 session_index 行失败: {error}"))?;
        let remove = serde_json::from_str::<Value>(&line)
            .ok()
            .and_then(|value| {
                value
                    .get("id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .is_some_and(|id| deleted.contains(&id));
        if !remove {
            kept.push(line);
        }
    }
    let tmp = path.with_extension("jsonl.2xapi-delete.tmp");
    let raw = if kept.is_empty() {
        String::new()
    } else {
        kept.join("\n") + "\n"
    };
    std::fs::write(&tmp, raw).map_err(|error| format!("写 session_index 临时文件失败: {error}"))?;
    private_file(&tmp)?;
    std::fs::rename(&tmp, &path).map_err(|error| format!("替换 session_index 失败: {error}"))?;
    Ok(())
}

pub fn apply_delete(codex_home: &Path, backup_dir: &Path, token: &str) -> Result<Value, String> {
    if codex_running() {
        return Err("Codex 正在运行，请先退出后再删除会话".into());
    }
    let plan = DELETE_PLANS
        .lock()
        .map_err(|_| "删除预览锁已损坏".to_string())?
        .remove(token)
        .ok_or_else(|| "删除确认已失效，请重新预览".to_string())?;
    if chrono::Utc::now().timestamp() - plan.created_at > 600 {
        return Err("删除确认已过期，请重新预览".into());
    }
    let _guard = HISTORY_WRITE_LOCK
        .lock()
        .map_err(|_| "历史写入锁损坏".to_string())?;
    invalidate_session_snapshot(codex_home);
    let state = state_rollouts(codex_home);
    for id in &plan.ids {
        let info = state
            .get(id)
            .ok_or_else(|| format!("会话状态已变化: {id}"))?;
        session_meta_record(&info.path, id)?;
    }
    let backup_id = create_history_backup(codex_home, backup_dir, "delete", &plan.ids, None, true)?;
    let result = (|| {
        let deleted = delete_from_databases(codex_home, &plan.ids)?;
        let deleted_ids: HashSet<_> = plan.ids.iter().cloned().collect();
        prune_session_index(codex_home, &deleted_ids)?;
        for id in &plan.ids {
            let info = state.get(id).ok_or_else(|| "会话路径丢失".to_string())?;
            std::fs::remove_file(&info.path)
                .map_err(|error| format!("移除 rollout 失败: {error}"))?;
        }
        Ok(deleted)
    })();
    match result {
        Ok(deleted) => Ok(json!({"deleted": deleted, "backupId": backup_id})),
        Err(error) => {
            let _ = restore_history_backup(codex_home, backup_dir, &backup_id);
            Err(error)
        }
    }
}

pub fn undo_delete(codex_home: &Path, backup_dir: &Path, id: &str) -> Result<Value, String> {
    if codex_running() {
        return Err("Codex 正在运行，请先退出后再撤销删除".into());
    }
    let _guard = HISTORY_WRITE_LOCK
        .lock()
        .map_err(|_| "历史写入锁损坏".to_string())?;
    invalidate_session_snapshot(codex_home);
    restore_history_backup(codex_home, backup_dir, id)
}

#[cfg(target_os = "macos")]
fn quit_codex() -> Result<(), String> {
    let status = std::process::Command::new("osascript")
        .args(["-e", "tell application id \"com.openai.codex\" to quit"])
        .status()
        .map_err(|error| format!("无法退出 Codex: {error}"))?;
    if !status.success() {
        return Err("系统拒绝退出 Codex".into());
    }
    for _ in 0..40 {
        if !codex_running() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err("Codex 未在预期时间内退出".into())
}

#[cfg(target_os = "macos")]
fn open_codex() -> Result<(), String> {
    std::process::Command::new("open")
        .args(["-b", "com.openai.codex"])
        .spawn()
        .map_err(|error| format!("无法重新打开 Codex: {error}"))?;
    Ok(())
}

pub fn restart_codex() -> Result<Value, String> {
    #[cfg(target_os = "macos")]
    {
        if codex_running() {
            quit_codex()?;
        }
        open_codex()?;
        Ok(json!({"restarted": true, "bundleId": "com.openai.codex"}))
    }
    #[cfg(not(target_os = "macos"))]
    Err("重新打开 Codex 目前仅支持 macOS".into())
}

/// 对账 rollout 文件与 db 记录:db 指向的 rollout 文件存在 → 归属可确认;缺失 → 记 missing;
/// 修复 missing_candidate 标记。写操作前整库备份。
pub fn repair_sessions(codex_home: &Path, backup_dir: &Path) -> Value {
    invalidate_session_snapshot(codex_home);
    let Some(db_path) = catalog_db_path(codex_home) else {
        return json!({ "fixed": 0, "scanned": 0, "error": "未找到会话数据库" });
    };

    // 写前创建 SQLite 一致性快照；失败必须中止，不能带着无备份继续 UPDATE。
    if let Err(error) = std::fs::create_dir_all(backup_dir) {
        return json!({ "fixed": 0, "scanned": 0, "error": format!("创建备份目录失败: {error}") });
    }
    let backup_name = format!(
        "sessions-{}-{}.db",
        chrono::Local::now().format("%Y%m%d-%H%M%S%.3f"),
        uuid::Uuid::new_v4()
    );
    let backup_path = backup_dir.join(&backup_name);

    let mut conn = match Connection::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            return json!({ "fixed": 0, "scanned": 0, "error": format!("打开数据库失败: {e}") })
        }
    };
    let catalog = "local_thread_catalog";
    if !has_column(&conn, catalog, "source_detail") {
        return json!({ "fixed": 0, "scanned": 0, "error": "不支持的 schema(缺 source_detail)" });
    }
    let Some(backup_utf8) = backup_path.to_str() else {
        return json!({ "fixed": 0, "scanned": 0, "error": "备份路径不是有效 UTF-8" });
    };
    if let Err(error) = conn.execute("VACUUM INTO ?1", [backup_utf8]) {
        return json!({ "fixed": 0, "scanned": 0, "error": format!("创建会话数据库快照失败: {error}") });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&backup_path, std::fs::Permissions::from_mode(0o600));
    }
    let state = state_rollouts(codex_home);
    let existing_ids: HashSet<String> = {
        let mut stmt = match conn
            .prepare("SELECT thread_id FROM local_thread_catalog WHERE host_id='local'")
        {
            Ok(stmt) => stmt,
            Err(error) => {
                return json!({"fixed":0,"scanned":0,"error":format!("读取 catalog ID 失败: {error}")})
            }
        };
        let result = stmt.query_map([], |row| row.get::<_, String>(0));
        match result {
            Ok(rows) => rows.flatten().collect(),
            Err(error) => {
                return json!({"fixed":0,"scanned":0,"error":format!("读取 catalog ID 失败: {error}")})
            }
        }
    };

    let mut scanned = 0u32;
    let mut fixed = 0u32;

    // 遍历全部 catalog 行,对账 rollout 文件存在性
    let sql = format!("SELECT host_id, thread_id, source_detail, missing_candidate FROM {catalog}");
    let ids_to_fix: Vec<(String, String, Option<String>, i64)> = {
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return json!({ "fixed": 0, "scanned": 0, "error": "查询失败" }),
        };
        let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        }) else {
            return json!({ "fixed": 0, "scanned": 0, "error": "查询失败" });
        };
        let mut fixes = Vec::new();
        for row in rows {
            let Ok((host_id, tid, detail, current_missing)) = row else {
                return json!({ "fixed": 0, "scanned": scanned, "error": "读取会话字段失败" });
            };
            scanned += 1;
            let direct = detail
                .as_deref()
                .and_then(|raw| valid_rollout_path(codex_home, raw));
            let resolved = direct
                .clone()
                .or_else(|| state.get(&tid).map(|info| info.path.clone()));
            let want_missing = if resolved.is_some() { 0 } else { 1 };
            let replacement = if direct.is_none() {
                resolved.map(|path| path.to_string_lossy().to_string())
            } else {
                None
            };
            if current_missing != Some(want_missing) || replacement.is_some() {
                fixes.push((host_id, tid, replacement, want_missing));
            }
        }
        fixes
    };

    // 主键为 (host_id, thread_id)，整批在一个事务内完成；任一失败则全部回滚。
    let transaction = match conn.transaction() {
        Ok(transaction) => transaction,
        Err(error) => {
            return json!({ "fixed": 0, "scanned": scanned, "error": format!("开启修复事务失败: {error}") })
        }
    };
    for (host_id, tid, replacement, want_missing) in &ids_to_fix {
        let upd = format!(
            "UPDATE {catalog} SET missing_candidate = ?1, source_detail = COALESCE(?2, source_detail) WHERE host_id = ?3 AND thread_id = ?4"
        );
        match transaction.execute(
            &upd,
            rusqlite::params![want_missing, replacement, host_id, tid],
        ) {
            Ok(1) => fixed += 1,
            Ok(_) => {
                return json!({ "fixed": 0, "scanned": scanned, "error": "会话记录在修复期间发生变化,已回滚" })
            }
            Err(error) => {
                return json!({ "fixed": 0, "scanned": scanned, "error": format!("修复写入失败,已回滚: {error}") })
            }
        }
    }
    let max_sequence: i64 = transaction
        .query_row(
            "SELECT COALESCE(MAX(observation_sequence),0) FROM local_thread_catalog WHERE host_id='local'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let mut sequence = max_sequence;
    for (thread_id, info) in &state {
        if existing_ids.contains(thread_id) {
            continue;
        }
        sequence += 1;
        let seconds = info.updated_at as f64 / 1000.0;
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO local_thread_catalog
             (host_id, thread_id, display_title, source_created_at, source_updated_at, cwd, source_kind, source_detail, model_provider, git_branch, observation_sequence, missing_candidate, thread_source, source_recency_at, pending_observed_title)
             VALUES ('local', ?1, ?2, ?3, ?3, ?4, 'vscode', ?5, ?6, NULL, ?7, 0, 'user', ?3, 0)",
            rusqlite::params![
                thread_id,
                if info.title.trim().is_empty() { "(无标题)" } else { &info.title },
                seconds,
                info.cwd,
                info.path.to_string_lossy().to_string(),
                if info.provider.trim().is_empty() { "custom" } else { &info.provider },
                sequence,
            ],
        );
        match inserted {
            Ok(1) => fixed += 1,
            Ok(_) => {}
            Err(error)
                if error.to_string().contains("thread_source")
                    || error.to_string().contains("source_recency_at") =>
            {
                let fallback = transaction.execute(
                    "INSERT OR IGNORE INTO local_thread_catalog
                     (host_id, thread_id, display_title, source_created_at, source_updated_at, cwd, source_kind, source_detail, model_provider, git_branch, observation_sequence, missing_candidate)
                     VALUES ('local', ?1, ?2, ?3, ?3, ?4, 'vscode', ?5, ?6, NULL, ?7, 0)",
                    rusqlite::params![thread_id, info.title, seconds, info.cwd, info.path.to_string_lossy().to_string(), info.provider, sequence],
                );
                match fallback {
                    Ok(1) => fixed += 1,
                    Ok(_) => {}
                    Err(error) => {
                        return json!({"fixed":0,"scanned":scanned,"error":format!("补 catalog 失败: {error}")})
                    }
                }
            }
            Err(error) => {
                return json!({"fixed":0,"scanned":scanned,"error":format!("补 catalog 失败: {error}")})
            }
        }
    }
    if fixed > 0 {
        let _ = transaction.execute(
            "UPDATE local_thread_catalog_metadata SET catalog_revision = catalog_revision + 1 WHERE id = 1",
            [],
        );
        if has_column(
            &transaction,
            "local_thread_catalog_sync_state",
            "last_full_reconciled_at",
        ) {
            let _ = transaction.execute(
                "UPDATE local_thread_catalog_sync_state SET last_full_reconciled_at = ?1 WHERE host_id = 'local'",
                [chrono::Utc::now().timestamp()],
            );
        }
    }
    if let Err(error) = transaction.commit() {
        return json!({ "fixed": 0, "scanned": scanned, "error": format!("提交修复失败: {error}") });
    }

    json!({ "fixed": fixed, "scanned": scanned, "stateRollouts": state.len() })
}

/// 按 thread id 查找可恢复的 rollout，供“继续”按钮使用。
pub fn resumable_session(codex_home: &Path, thread_id: &str) -> Result<Value, String> {
    let id = uuid::Uuid::parse_str(thread_id.trim()).map_err(|_| "会话 ID 无效".to_string())?;
    let mut info = state_rollouts(codex_home).remove(&id.to_string());
    if info.is_none() {
        let Some(db_path) = catalog_db_path(codex_home) else {
            return Err("找不到可恢复的 rollout 文件（可能是实时语音或已被删除）".into());
        };
        let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| "无法读取会话 catalog".to_string())?;
        let raw: Option<String> = conn
            .query_row(
                "SELECT source_detail FROM local_thread_catalog WHERE thread_id=?1 LIMIT 1",
                [id.to_string()],
                |row| row.get(0),
            )
            .map_err(|_| "找不到该会话".to_string())?;
        let path = raw.and_then(|value| valid_rollout_path(codex_home, &value));
        info = path.map(|path| RolloutInfo {
            path,
            cwd: String::new(),
            title: String::new(),
            provider: String::new(),
            updated_at: 0,
            archived: false,
        });
    }
    let info =
        info.ok_or_else(|| "找不到可恢复的 rollout 文件（可能是实时语音或已被删除）".to_string())?;
    Ok(json!({
        "id": id.to_string(),
        "rolloutPath": info.path,
        "cwd": info.cwd,
        "title": info.title,
        "provider": info.provider,
        "updatedAt": info.updated_at,
        "archived": info.archived,
        "command": format!("codex resume {}", id),
    }))
}

/// 在 macOS Terminal 中打开固定的 `codex resume <id>`，不接受前端传入命令。
pub fn resume_session(codex_home: &Path, thread_id: &str) -> Result<Value, String> {
    let session = resumable_session(codex_home, thread_id)?;
    #[cfg(target_os = "macos")]
    {
        let cwd = session["cwd"].as_str().unwrap_or("");
        let id = session["id"].as_str().unwrap_or("");
        let codex_bin = crate::launcher::resolve_codex_bin();
        let codex_cmd = if Path::new(&codex_bin).is_file() || codex_bin == "codex" {
            shell_quote(&codex_bin)
        } else {
            return Err(format!("未找到 Codex CLI：{}", codex_bin));
        };
        let command = if cwd.is_empty() {
            format!("{codex_cmd} resume {id}")
        } else {
            format!("cd {} && {codex_cmd} resume {id}", shell_quote(cwd))
        };
        let script = format!(
            "tell application \"Terminal\"\nactivate\ndo script {}\nend tell\n",
            applescript_quote(&command)
        );
        let mut child = std::process::Command::new("/usr/bin/osascript")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("无法打开 Terminal：{error}"))?;
        child
            .stdin
            .take()
            .ok_or_else(|| "无法写入 Terminal 启动指令".to_string())?
            .write_all(script.as_bytes())
            .map_err(|error| format!("写入 Terminal 启动指令失败：{error}"))?;
        let output = child
            .wait_with_output()
            .map_err(|error| format!("等待 Terminal 响应失败：{error}"))?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(if detail.is_empty() {
                "系统拒绝打开 Terminal，请检查自动化权限".into()
            } else {
                detail
            });
        }
    }
    #[cfg(not(target_os = "macos"))]
    return Err("一键继续会话目前仅支持 macOS Terminal".into());
    Ok(session)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(target_os = "macos")]
fn applescript_quote(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

// ── autoRepairBeforeHost 设置 ───────────────────────────────

fn settings_path(codex_home: &Path) -> PathBuf {
    codex_home.join("2xapi-settings.json")
}

fn read_settings(codex_home: &Path) -> Value {
    std::fs::read_to_string(settings_path(codex_home))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| json!({}))
}

fn write_settings(codex_home: &Path, value: &Value) -> Result<(), String> {
    let path = settings_path(codex_home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("创建设置目录失败: {error}"))?;
    }
    let raw =
        serde_json::to_vec_pretty(value).map_err(|error| format!("编码会话设置失败: {error}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, raw).map_err(|error| format!("写会话设置失败: {error}"))?;
    private_file(&tmp)?;
    std::fs::rename(&tmp, &path).map_err(|error| format!("保存会话设置失败: {error}"))?;
    private_file(&path)
}

pub fn get_settings(codex_home: &Path) -> Value {
    let settings = read_settings(codex_home);
    let enabled = settings
        .get("autoRepairBeforeLaunch")
        .or_else(|| settings.get("autoRepairBeforeHost"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    json!({"autoRepairBeforeLaunch": enabled})
}

pub fn set_settings(codex_home: &Path, auto_repair: bool) -> Result<Value, String> {
    let mut settings = read_settings(codex_home);
    let object = settings
        .as_object_mut()
        .ok_or_else(|| "2xapi-settings.json 顶层必须是对象".to_string())?;
    object.remove("autoRepairBeforeHost");
    object.insert("autoRepairBeforeLaunch".into(), json!(auto_repair));
    write_settings(codex_home, &settings)?;
    Ok(json!({"autoRepairBeforeLaunch": auto_repair}))
}

pub fn auto_repair_before_launch(codex_home: &Path, backup_dir: &Path) -> Result<(), String> {
    if get_settings(codex_home)
        .get("autoRepairBeforeLaunch")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let result = repair_sessions(codex_home, backup_dir);
        if let Some(error) = result.get("error").and_then(Value::as_str) {
            return Err(error.to_string());
        }
    }
    Ok(())
}

// ── 单测 ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    fn sandbox(label: &str) -> (PathBuf, PathBuf) {
        let n = N.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("2xapi-stage3-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("codex");
        std::fs::create_dir_all(home.join("sqlite")).unwrap();
        (home, root.join("backups"))
    }

    /// 构造一个与真实 catalog 同 schema 的内存 db(写文件)。
    fn make_catalog_db(root: &Path, rows: &[(&str, &str, &str, &str, i64)]) -> PathBuf {
        let db_path = root.join("sqlite/codex-dev.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE local_thread_catalog (
                host_id TEXT NOT NULL, thread_id TEXT NOT NULL, display_title TEXT NOT NULL,
                source_created_at REAL NOT NULL, source_updated_at REAL NOT NULL, cwd TEXT NOT NULL,
                source_kind TEXT NOT NULL, source_detail TEXT, model_provider TEXT NOT NULL,
                git_branch TEXT, observation_sequence INTEGER NOT NULL,
                missing_candidate INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (host_id, thread_id));",
        )
        .unwrap();
        for (tid, title, provider, detail, updated_sec) in rows {
            conn.execute(
                "INSERT INTO local_thread_catalog
                 (host_id, thread_id, display_title, source_created_at, source_updated_at, cwd, source_kind, source_detail, model_provider, git_branch, observation_sequence)
                 VALUES ('local', ?1, ?2, 0, ?5, '/tmp/proj', 'vscode', ?4, ?3, NULL, 0)",
                rusqlite::params![tid, title, provider, detail, *updated_sec as f64],
            )
            .unwrap();
        }
        db_path
    }

    #[test]
    fn list_sessions_paginated_and_provider_filtered() {
        let (root, _bk) = sandbox("list");
        // 两个 provider、不同时间
        let rollout = root.join("sessions/2026/01/r.jsonl");
        std::fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        let _ = std::fs::write(&rollout, "{}");
        make_catalog_db(
            &root,
            &[
                ("t1", "会话甲", "custom", rollout.to_str().unwrap(), 1000),
                ("t2", "会话乙", "2xapi", "", 3000),
                ("t3", "会话丙", "custom", "", 2000),
            ],
        );
        let home = &root;

        eprintln!(
            "[DBG] db_path exists: {}",
            home.join("sqlite/codex-dev.db").exists()
        );
        let r = list_sessions(home, 1, 10, "");
        eprintln!("[DBG] list result: {}", r);
        assert_eq!(r["total"], 3);
        let items = r["items"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["id"], "t2", "应按 updatedAt 倒序");
        assert_eq!(items[0]["providerTag"], "2xapi");
        assert_eq!(items[1]["title"], "会话丙");

        // provider 过滤
        let r2 = list_sessions(home, 1, 10, "custom");
        assert_eq!(r2["total"], 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn repair_marks_missing_and_clears() {
        let (root, bk) = sandbox("repair");
        let good = root.join("sessions/2026/01/good.jsonl");
        std::fs::create_dir_all(good.parent().unwrap()).unwrap();
        let _ = std::fs::write(&good, "{}");
        // t1 文件存在(missing 应为 0);t2 文件不存在(missing 应为 1);t3 文件存在但标记了 1(应清 0)
        make_catalog_db(
            &root,
            &[
                ("t1", "甲", "custom", good.to_str().unwrap(), 100),
                ("t2", "乙", "custom", "/nonexistent/x.jsonl", 200),
                ("t3", "丙", "custom", good.to_str().unwrap(), 300),
            ],
        );
        // t3 手工标 missing=1
        let conn = Connection::open(root.join("sqlite/codex-dev.db")).unwrap();
        conn.execute(
            "UPDATE local_thread_catalog SET missing_candidate=1 WHERE thread_id='t3'",
            [],
        )
        .unwrap();
        drop(conn);

        let r = repair_sessions(&root, &bk);
        assert_eq!(r["scanned"], 3, "应扫描全部");
        assert_eq!(
            r["fixed"], 2,
            "t2 标 missing + t3 清 missing 共 2 行修正;t1 本正确不动"
        );
        // 备份已建
        assert!(
            std::fs::read_dir(&bk).unwrap().next().is_some(),
            "写前应有整库备份"
        );

        // 验证落库
        let conn = Connection::open(root.join("sqlite/codex-dev.db")).unwrap();
        let get = |tid: &str| -> i64 {
            conn.query_row(
                "SELECT missing_candidate FROM local_thread_catalog WHERE thread_id=?1",
                [tid],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(get("t1"), 0);
        assert_eq!(get("t2"), 1, "缺失 rollout 应标记 missing");
        assert_eq!(get("t3"), 0, "存在文件应清除 missing");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn repair_stops_when_backup_cannot_be_created() {
        let (root, _) = sandbox("repair-backup-fail");
        make_catalog_db(
            &root,
            &[("t1", "甲", "custom", "/nonexistent/x.jsonl", 100)],
        );
        let invalid_backup_dir = root.join("not-a-directory");
        std::fs::write(&invalid_backup_dir, "file").unwrap();

        let result = repair_sessions(&root, &invalid_backup_dir);
        assert!(result["error"].as_str().unwrap().contains("备份目录"));
        let conn = Connection::open(root.join("sqlite/codex-dev.db")).unwrap();
        let missing: i64 = conn
            .query_row(
                "SELECT missing_candidate FROM local_thread_catalog WHERE host_id='local' AND thread_id='t1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(missing, 0, "备份失败时不得继续修复写入");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn repair_backfills_null_source_detail_from_state_db() {
        let (root, bk) = sandbox("state-fallback");
        let id = "019ffd83-2e0c-7a33-bb3e-b840519f15f9";
        let rollout = root.join(format!(
            "sessions/2026/01/rollout-2026-01-01T00-00-00-{id}.jsonl"
        ));
        std::fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        std::fs::write(&rollout, "{}").unwrap();
        make_catalog_db(&root, &[(id, "甲", "custom", "", 100)]);
        let catalog = Connection::open(root.join("sqlite/codex-dev.db")).unwrap();
        catalog
            .execute(
                "UPDATE local_thread_catalog SET source_detail=NULL, missing_candidate=1 WHERE thread_id=?1",
                [id],
            )
            .unwrap();
        drop(catalog);
        let state = root.join("state_5.sqlite");
        let state_conn = Connection::open(&state).unwrap();
        state_conn
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL, source TEXT NOT NULL, model_provider TEXT NOT NULL,
                    cwd TEXT NOT NULL, title TEXT NOT NULL, preview TEXT NOT NULL, archived INTEGER NOT NULL,
                    thread_source TEXT
                );",
            )
            .unwrap();
        state_conn
            .execute(
                "INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, preview, archived, thread_source)
                 VALUES (?1, ?2, 0, 1, 'cli', 'custom', '/tmp/project', '甲', '甲', 0, 'user')",
                rusqlite::params![id, rollout.to_string_lossy().to_string()],
            )
            .unwrap();

        let result = repair_sessions(&root, &bk);
        assert_eq!(result["fixed"], 1);
        let conn = Connection::open(root.join("sqlite/codex-dev.db")).unwrap();
        let (missing, detail): (i64, String) = conn
            .query_row(
                "SELECT missing_candidate, source_detail FROM local_thread_catalog WHERE thread_id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(missing, 0);
        assert_eq!(detail, rollout.canonicalize().unwrap().to_string_lossy());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resumable_session_uses_state_rollout_path() {
        let (root, _bk) = sandbox("resume");
        let id = "019ffd83-2e0c-7a33-bb3e-b840519f15f9";
        let rollout = root.join(format!(
            "sessions/2026/01/rollout-2026-01-01T00-00-00-{id}.jsonl"
        ));
        std::fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        std::fs::write(&rollout, "{}").unwrap();
        let state = root.join("state_5.sqlite");
        let state_conn = Connection::open(&state).unwrap();
        state_conn
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL, source TEXT NOT NULL, model_provider TEXT NOT NULL,
                    cwd TEXT NOT NULL, title TEXT NOT NULL, preview TEXT NOT NULL, archived INTEGER NOT NULL,
                    thread_source TEXT
                );",
            )
            .unwrap();
        state_conn
            .execute(
                "INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, preview, archived, thread_source)
                 VALUES (?1, ?2, 0, 1, 'cli', 'custom', '/tmp/project', '甲', '甲', 0, 'user')",
                rusqlite::params![id, rollout.to_string_lossy().to_string()],
            )
            .unwrap();
        let result = resumable_session(&root, id).unwrap();
        assert_eq!(result["id"], id);
        assert_eq!(result["command"], format!("codex resume {id}"));
        let _ = std::fs::remove_dir_all(&root);
    }

    fn make_state_db(root: &Path, id: &str, rollout: &Path, archived: bool) -> PathBuf {
        let path = root.join("state_5.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL, source TEXT NOT NULL, model_provider TEXT NOT NULL,
                cwd TEXT NOT NULL, title TEXT NOT NULL, preview TEXT NOT NULL, archived INTEGER NOT NULL,
                thread_source TEXT
            );
            CREATE TABLE thread_dynamic_tools (thread_id TEXT NOT NULL, position INTEGER NOT NULL, name TEXT, PRIMARY KEY(thread_id,position));
            CREATE TABLE thread_spawn_edges (parent_thread_id TEXT NOT NULL, child_thread_id TEXT NOT NULL PRIMARY KEY, status TEXT NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, preview, archived, thread_source)
             VALUES (?1, ?2, 0, 1, 'cli', 'custom', '/tmp/project', '甲', '甲', ?3, 'user')",
            rusqlite::params![id, rollout.to_string_lossy().to_string(), archived as i64],
        )
        .unwrap();
        path
    }

    #[test]
    fn delete_backup_can_restore_db_and_rollout() {
        let (root, backup_dir) = sandbox("delete-undo");
        let id = "019ffd83-2e0c-7a33-bb3e-b840519f15f9";
        let rollout = root.join(format!(
            "sessions/2026/01/rollout-2026-01-01T00-00-00-{id}.jsonl"
        ));
        std::fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        std::fs::write(
            &rollout,
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{id}","model_provider":"custom"}}}}"#
            ),
        )
        .unwrap();
        make_state_db(&root, id, &rollout, false);
        make_catalog_db(&root, &[(id, "甲", "custom", rollout.to_str().unwrap(), 1)]);
        let backup_id =
            create_history_backup(&root, &backup_dir, "delete", &[id.to_string()], None, true)
                .unwrap();
        delete_from_databases(&root, &[id.to_string()]).unwrap();
        std::fs::remove_file(&rollout).unwrap();
        assert!(!rollout.exists());
        restore_history_backup(&root, &backup_dir, &backup_id).unwrap();
        assert!(rollout.exists());
        let conn = Connection::open(root.join("state_5.sqlite")).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM threads WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn list_page_uses_real_archive_state() {
        let (root, _backup) = sandbox("page-stats");
        let id = "019ffd83-2e0c-7a33-bb3e-b840519f15f9";
        let rollout = root.join(format!("archived_sessions/rollout-{id}.jsonl"));
        std::fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        std::fs::write(
            &rollout,
            format!(r#"{{"type":"session_meta","payload":{{"id":"{id}"}}}}"#),
        )
        .unwrap();
        make_state_db(&root, id, &rollout, true);
        let page = list_sessions_page(&root, 1, 50);
        assert_eq!(page["total"], 1);
        assert_eq!(page["pageStats"]["archived"], 1);
        assert_eq!(page["items"][0]["archived"], true);
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn snapshot_paginates_and_invalidates() {
        let (root, _backup) = sandbox("snapshot");
        let id = "019ffd83-2e0c-7a33-bb3e-b840519f15f9";
        let rollout = root.join(format!("sessions/2026/01/rollout-{id}.jsonl"));
        std::fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        std::fs::write(
            &rollout,
            format!(r#"{{"type":"session_meta","payload":{{"id":"{id}"}}}}"#),
        )
        .unwrap();
        make_state_db(&root, id, &rollout, false);

        let inspect = inspect_sessions(&root);
        let snapshot_id = inspect["snapshotId"].as_str().unwrap();
        let page = list_sessions_page_from_snapshot(&root, snapshot_id, 1, 50).unwrap();
        assert_eq!(page["total"], 1);
        assert_eq!(page["items"][0]["id"], id);

        invalidate_session_snapshot(&root);
        assert!(list_sessions_page_from_snapshot(&root, snapshot_id, 1, 50).is_err());
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn ghost_index_removes_only_strict_unreferenced_rows() {
        let (root, _backup) = sandbox("ghost-index");
        let live_id = "019ffd83-2e0c-7a33-bb3e-b840519f15f9";
        let rollout = root.join(format!("sessions/2026/01/rollout-{live_id}.jsonl"));
        std::fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        std::fs::write(
            &rollout,
            format!(r#"{{"type":"session_meta","payload":{{"id":"{live_id}"}}}}"#),
        )
        .unwrap();
        let raw = format!(
            "{{\"id\":\"ghost\",\"thread_name\":\"Ghost\",\"updated_at\":\"1\"}}\r\nnot-json\r\n{{\"id\":\"{live_id}\",\"thread_name\":\"Live\",\"updated_at\":\"2\"}}\r\n{{\"id\":\"extra\",\"thread_name\":\"Extra\",\"updated_at\":\"3\",\"other\":true}}\r\n"
        );
        let index = root.join("session_index.jsonl");
        std::fs::write(&index, raw).unwrap();

        assert_eq!(prune_ghost_session_index(&root).unwrap(), 1);
        let kept = std::fs::read_to_string(&index).unwrap();
        assert_eq!(
            kept,
            format!(
                "not-json\r\n{{\"id\":\"{live_id}\",\"thread_name\":\"Live\",\"updated_at\":\"2\"}}\r\n{{\"id\":\"extra\",\"thread_name\":\"Extra\",\"updated_at\":\"3\",\"other\":true}}\r\n"
            )
        );
        assert_eq!(prune_ghost_session_index(&root).unwrap(), 0);
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn settings_default_off_and_roundtrip() {
        let (root, _bk) = sandbox("settings");
        assert!(
            !get_settings(&root)["autoRepairBeforeLaunch"]
                .as_bool()
                .unwrap(),
            "默认关闭自动修复"
        );
        set_settings(&root, true).unwrap();
        assert!(get_settings(&root)["autoRepairBeforeLaunch"]
            .as_bool()
            .unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }
}
