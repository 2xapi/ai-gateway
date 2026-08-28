//! Cursor 接入(F 阶段正式化;规格见《Cursor批次任务书草案.md》+《Cursor接入调研草案.md》)。
//! 载体:`{home}/Library/Application Support/Cursor/User/globalStorage/state.vscdb`(SQLite ItemTable)。
//! 写入点 1:键 `persistentStorage.applicationUser` 的 JSON 值内 `aiSettings.{useOpenAIKey, openAIBaseUrl}`
//! 写入点 2:键 `cursorAuth/openAIKey` = 明文 Key(Cursor 启动自动迁移进系统钥匙串并删明文)
//! 三保险:写前备份 vscdb(config::backup_file)+ 快照存原 JSON(backup_dir/cursor-ai-before.json,unhost 精确还原)+ SQLite 事务写
//! 铁律:真实 Cursor 配置是用户在用的活配置(含登录态),测试一律 tempdir;**Cursor 运行中拒绝写**
//! (运行中写会被退出时的内存覆盖;实证:退出时写、写后 5 分钟稳定,未触发「开关自动回跳」bug)

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub type OpError = (u16, String, String);

fn vscdb_path(home: &Path) -> PathBuf {
    if cfg!(windows) {
        std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join("AppData/Roaming"))
            .join("Cursor/User/globalStorage/state.vscdb")
    } else if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Cursor/User/globalStorage/state.vscdb")
    } else {
        std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".config"))
            .join("Cursor/User/globalStorage/state.vscdb")
    }
}

fn sqlite_sidecar_path(db: &Path, suffix: &str) -> PathBuf {
    let mut path = db.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

const AI_SETTINGS_KEY: &str = "src.vs.platform.reactivestorage.browser.reactiveStorageServiceImpl.persistentStorage.applicationUser";
const OPENAI_KEY_KEY: &str = "cursorAuth/openAIKey";
const MANAGED_KEY: &str = "2xapi/cursorManaged";
const SNAPSHOT_NAME: &str = "cursor-ai-before.json";

/// Cursor 主进程是否在运行(写前必须退出,运行中写会被覆盖)。
fn cursor_running() -> bool {
    #[cfg(windows)]
    {
        return std::process::Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq Cursor.exe", "/NH"])
            .output()
            .map(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .to_ascii_lowercase()
                    .contains("cursor.exe")
            })
            .unwrap_or(false);
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("pgrep")
            .args(["-x", "Cursor"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

// ── ItemTable 读写(只碰目标键)───────────────────────────

fn open_db(db: &Path) -> Result<rusqlite::Connection, OpError> {
    rusqlite::Connection::open(db)
        .map_err(|e| (500, "E_CURSOR_DB".into(), format!("打开 vscdb 失败: {e}")))
}

fn read_item(db: &Path, key: &str) -> Result<Option<String>, OpError> {
    let conn = open_db(db)?;
    let mut stmt = conn
        .prepare("SELECT value FROM ItemTable WHERE key = ?1")
        .map_err(|e| (500, "E_CURSOR_DB".into(), e.to_string()))?;
    let mut rows = stmt
        .query_map(rusqlite::params![key], |r| r.get::<_, String>(0))
        .map_err(|e| (500, "E_CURSOR_DB".into(), e.to_string()))?;
    match rows.next() {
        Some(Ok(v)) => Ok(Some(v)),
        Some(Err(e)) => Err((500, "E_CURSOR_DB".into(), e.to_string())),
        None => Ok(None),
    }
}

#[cfg(test)]
fn upsert_item(db: &Path, key: &str, value: &str) -> Result<(), OpError> {
    let exists = read_item(db, key)?.is_some();
    let conn = open_db(db)?;
    let n = if exists {
        conn.execute(
            "UPDATE ItemTable SET value = ?2 WHERE key = ?1",
            rusqlite::params![key, value],
        )
    } else {
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )
    }
    .map_err(|e| (500, "E_CURSOR_DB".into(), e.to_string()))?;
    debug_assert_eq!(n, 1);
    Ok(())
}

fn write_managed_transaction(
    db: &Path,
    ai: &Value,
    key: Option<&str>,
    marker: Option<&str>,
) -> Result<(), OpError> {
    let mut conn = open_db(db)?;
    let tx = conn
        .transaction()
        .map_err(|e| (500, "E_CURSOR_DB".into(), e.to_string()))?;
    tx.execute(
        "INSERT INTO ItemTable (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        rusqlite::params![AI_SETTINGS_KEY, ai.to_string()],
    )
    .map_err(|e| (500, "E_CURSOR_DB".into(), e.to_string()))?;
    match key {
        Some(value) => {
            tx.execute(
                "INSERT INTO ItemTable (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                rusqlite::params![OPENAI_KEY_KEY, value],
            )
            .map_err(|e| (500, "E_CURSOR_DB".into(), e.to_string()))?;
        }
        None => {
            tx.execute(
                "DELETE FROM ItemTable WHERE key = ?1",
                rusqlite::params![OPENAI_KEY_KEY],
            )
            .map_err(|e| (500, "E_CURSOR_DB".into(), e.to_string()))?;
        }
    }
    match marker {
        Some(value) => {
            tx.execute(
                "INSERT INTO ItemTable (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                rusqlite::params![MANAGED_KEY, value],
            )
            .map_err(|e| (500, "E_CURSOR_DB".into(), e.to_string()))?;
        }
        None => {
            tx.execute(
                "DELETE FROM ItemTable WHERE key = ?1",
                rusqlite::params![MANAGED_KEY],
            )
            .map_err(|e| (500, "E_CURSOR_DB".into(), e.to_string()))?;
        }
    }
    tx.commit()
        .map_err(|e| (500, "E_CURSOR_DB".into(), e.to_string()))
}

fn read_ai_settings(db: &Path) -> Result<Value, OpError> {
    match read_item(db, AI_SETTINGS_KEY)? {
        Some(raw) => serde_json::from_str(&raw).map_err(|e| {
            (
                500,
                "E_CURSOR_DB".into(),
                format!("aiSettings 非 JSON: {e}"),
            )
        }),
        None => Ok(json!({})),
    }
}

#[cfg(test)]
fn write_ai_settings(db: &Path, v: &Value) -> Result<(), OpError> {
    upsert_item(db, AI_SETTINGS_KEY, &v.to_string())
}

/// aiSettings JSON 内取/建 `aiSettings` 子对象(调研实证:该键值为大 JSON,aiSettings 是其顶层段)。
fn ai_settings_mut(ai: &mut Value) -> Result<&mut Map<String, Value>, OpError> {
    let obj = ai.as_object_mut().ok_or((
        500,
        "E_CURSOR_DB".into(),
        "aiSettings 值不是 JSON 对象".into(),
    ))?;
    obj.entry("aiSettings")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or((500, "E_CURSOR_DB".into(), "aiSettings 段不是对象".into()))
}

/// 快照路径(host 前的 aiSettings 原 JSON,unhost 精确还原用)。
fn snapshot_path(backup_dir: &Path) -> PathBuf {
    backup_dir.join(SNAPSHOT_NAME)
}

/// GET /api/desktop/cursor/state —— 安装/运行/托管态(安装检测只作 UX 提示,不门控)。
pub fn state(home: &Path) -> Value {
    let db = vscdb_path(home);
    let installed = db.exists();
    let mut hosting = Value::Null;
    if installed {
        if let Ok(Some(raw)) = read_item(&db, AI_SETTINGS_KEY) {
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                let base = v
                    .pointer("/aiSettings/openAIBaseUrl")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                // DB 内私有标记是本产品托管凭据，避免把用户手配的同地址误判为托管。
                if read_item(&db, MANAGED_KEY).ok().flatten().is_some() {
                    hosting = json!({
                        "way": if base.contains(crate::desktop::GATEWAY_ADDR) { "gateway" } else { "direct" },
                        "baseUrl": base
                    });
                }
            }
        }
    }
    json!({
        "installed": installed,
        "running": cursor_running(),
        "hosting": hosting,
        "vscdb": db.to_string_lossy(),
    })
}

/// POST /api/desktop/cursor/host {providerId, way} —— 外科手术式写 aiSettings 两字段 + Key 明文。
/// way=gateway:base 指网关 /v1、Key=占位(零真 Key 落盘);way=direct:base=供应商地址(/v1 尾规则)、Key 落盘。
pub fn host(
    home: &Path,
    backup_dir: &Path,
    providers_path: &Path,
    provider_id: &str,
    way: &str,
) -> Result<Value, OpError> {
    if way != "gateway" && way != "direct" {
        return Err((
            400,
            "E_BAD_WAY".into(),
            "未知托管方式,仅支持 gateway / direct".into(),
        ));
    }
    let db = vscdb_path(home);
    if !db.exists() {
        return Err((
            404,
            "E_CURSOR_NOT_FOUND".into(),
            "未找到 Cursor 的 state.vscdb,请先安装 Cursor 并启动过一次".into(),
        ));
    }
    if cursor_running() {
        return Err((
            409,
            "E_CURSOR_RUNNING".into(),
            "Cursor 正在运行,请先退出 Cursor 再托管(运行中写入会被退出覆盖)".into(),
        ));
    }
    let data = crate::providers::load(providers_path);
    let provider = data
        .providers
        .iter()
        .find(|p| p.id == provider_id)
        .cloned()
        .ok_or_else(|| (404, "E_PROVIDER_NOT_FOUND".into(), "找不到该供应商".into()))?;
    crate::desktop::validate_provider_agent(&provider, "cursor")?;
    if provider.model.is_empty() {
        return Err((
            422,
            "E_NO_MODEL".into(),
            "该供应商未配置默认模型,请先在编辑里拉取模型或手填".into(),
        ));
    }

    let before = read_ai_settings(&db)?;
    let before_key = read_item(&db, OPENAI_KEY_KEY)?;
    let snapshot_file = snapshot_path(backup_dir);
    let paths = [
        db.clone(),
        sqlite_sidecar_path(&db, "-wal"),
        sqlite_sidecar_path(&db, "-shm"),
        snapshot_file.clone(),
        providers_path.to_path_buf(),
    ];
    let snapshots = paths
        .iter()
        .map(|path| crate::desktop::snapshot_file(path).map(|snapshot| (path.clone(), snapshot)))
        .collect::<Result<Vec<_>, _>>()?;
    // 三保险:①整体备份 vscdb ②快照原 JSON ③SQLite 事务写；失败恢复 DB/快照/active。
    std::fs::create_dir_all(backup_dir).map_err(|e| (500, "E_IO".into(), e.to_string()))?;
    crate::config::backup_file(&db, backup_dir, "cursor-vscdb", "pre-host")
        .map_err(|e| (500, "E_IO".into(), e))?;

    // 写 aiSettings 两字段
    let mut ai = before.clone();
    let settings = ai_settings_mut(&mut ai)?;
    let base = if way == "gateway" {
        format!("http://{}/v1", crate::desktop::GATEWAY_ADDR)
    } else {
        let b = provider.base_url.trim_end_matches('/').to_string();
        if b.ends_with("/v1") {
            b
        } else {
            format!("{b}/v1")
        }
    };
    settings.insert("useOpenAIKey".into(), json!(true));
    settings.insert("openAIBaseUrl".into(), json!(base));
    // Key 明文回退(启动自动迁移进钥匙串并删明文;gateway=占位零真 Key)
    let key = if way == "gateway" {
        "2xapi-gateway-managed".to_string()
    } else {
        provider.api_key.clone()
    };
    let outcome = (|| {
        // 快照仅首次托管时记录受控字段和原明文 Key；不快照整个 applicationUser。
        if !snapshot_file.exists() {
            let ai_settings = before.get("aiSettings").and_then(Value::as_object);
            let snap = json!({
                "version": 2,
                "fields": {
                    "useOpenAIKey": ai_settings.and_then(|settings| settings.get("useOpenAIKey")).cloned().unwrap_or(Value::Null),
                    "useOpenAIKeyPresent": ai_settings.is_some_and(|settings| settings.contains_key("useOpenAIKey")),
                    "openAIBaseUrl": ai_settings.and_then(|settings| settings.get("openAIBaseUrl")).cloned().unwrap_or(Value::Null),
                    "openAIBaseUrlPresent": ai_settings.is_some_and(|settings| settings.contains_key("openAIBaseUrl")),
                },
                "openAIKey": before_key.clone(),
                "openAIKeyPresent": before_key.is_some(),
            });
            std::fs::write(&snapshot_file, snap.to_string())
                .map_err(|e| (500, "E_IO".into(), e.to_string()))?;
        }
        write_managed_transaction(
            &db,
            &ai,
            Some(&key),
            Some(&json!({ "way": way, "providerId": provider.id }).to_string()),
        )?;
        crate::desktop::set_active_checked(providers_path, &provider, "cursor")?;
        Ok(json!({
            "hosted": true,
            "way": way,
            "baseUrl": base,
            "restart": true,
            "hint": "Cursor 托管已写入;重启 Cursor 后自动读入(Key 迁移进系统钥匙串)。托管期间请保持 Cursor 关闭时再修改。",
        }))
    })();
    outcome.map_err(|error| crate::desktop::rollback_files(error, &snapshots))
}

/// POST /api/desktop/cursor/unhost —— 快照还原(aiSettings 回原 JSON + 删 Key 明文键);无快照=摘除受控字段。
pub fn unhost(home: &Path, backup_dir: &Path) -> Result<Value, OpError> {
    let db = vscdb_path(home);
    let providers_path = crate::desktop::providers_path_from_backup_dir(backup_dir);
    if !db.exists() {
        crate::desktop::clear_active_checked(&providers_path, "cursor")?;
        return Ok(json!({ "hosted": false, "changed": false }));
    }
    if cursor_running() {
        return Err((
            409,
            "E_CURSOR_RUNNING".into(),
            "Cursor 正在运行,请先退出 Cursor 再还原".into(),
        ));
    }
    let snapshot_file = snapshot_path(backup_dir);
    let paths = [
        db.clone(),
        sqlite_sidecar_path(&db, "-wal"),
        sqlite_sidecar_path(&db, "-shm"),
        snapshot_file.clone(),
        providers_path.clone(),
    ];
    let snapshots = paths
        .iter()
        .map(|path| crate::desktop::snapshot_file(path).map(|snapshot| (path.clone(), snapshot)))
        .collect::<Result<Vec<_>, _>>()?;
    let outcome = (|| {
        let snap = match std::fs::read_to_string(&snapshot_file) {
            Ok(raw) => Some(raw),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err((500, "E_IO".into(), error.to_string())),
        };
        let before = read_ai_settings(&db)?;
        let before_key = read_item(&db, OPENAI_KEY_KEY)?;
        let marker = read_item(&db, MANAGED_KEY)?;
        let mut current = before.clone();
        let mut restored_key = before_key.clone();
        match snap.as_deref() {
            Some(raw) => {
                let snapshot = serde_json::from_str::<Value>(raw)
                    .map_err(|e| (500, "E_CURSOR_DB".into(), format!("快照损坏: {e}")))?;
                let settings = ai_settings_mut(&mut current)?;
                let fields = if snapshot.get("version").and_then(Value::as_u64) == Some(2) {
                    snapshot.get("fields").unwrap_or(&Value::Null)
                } else {
                    snapshot.pointer("/aiSettings").unwrap_or(&Value::Null)
                };
                for (field, present_key) in [
                    ("useOpenAIKey", "useOpenAIKeyPresent"),
                    ("openAIBaseUrl", "openAIBaseUrlPresent"),
                ] {
                    let present = if snapshot.get("version").and_then(Value::as_u64) == Some(2) {
                        fields
                            .get(present_key)
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                    } else {
                        fields.get(field).is_some()
                    };
                    if present {
                        settings.insert(
                            field.into(),
                            fields.get(field).cloned().unwrap_or(Value::Null),
                        );
                    } else {
                        settings.remove(field);
                    }
                }
                restored_key = if snapshot
                    .get("openAIKeyPresent")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    snapshot
                        .get("openAIKey")
                        .and_then(Value::as_str)
                        .map(String::from)
                } else {
                    None
                };
            }
            None => {
                let settings = ai_settings_mut(&mut current)?;
                let base = settings
                    .get("openAIBaseUrl")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let ours = marker.is_some()
                    || (settings
                        .get("useOpenAIKey")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                        && base.contains(crate::desktop::GATEWAY_ADDR));
                if ours {
                    settings.remove("useOpenAIKey");
                    settings.remove("openAIBaseUrl");
                    if before_key.as_deref() == Some("2xapi-gateway-managed") || marker.is_some() {
                        restored_key = None;
                    }
                }
            }
        }
        let changed = before != current || before_key != restored_key || marker.is_some();
        if changed {
            write_managed_transaction(&db, &current, restored_key.as_deref(), None)?;
        }
        crate::desktop::clear_active_checked(&providers_path, "cursor")?;
        if snapshot_file.exists() {
            std::fs::remove_file(&snapshot_file)
                .map_err(|error| (500, "E_IO".into(), error.to_string()))?;
        }
        Ok(json!({ "hosted": false, "changed": changed }))
    })();
    outcome.map_err(|error| crate::desktop::rollback_files(error, &snapshots))
}

// ── 单测(隔离 tempdir 构造 vscdb;真实 Cursor 零触碰)─────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn spec_db(root: &Path) -> PathBuf {
        let dir = root.join("Library/Application Support/Cursor/User/globalStorage");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("state.vscdb");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT, ts INTEGER)",
            [],
        )
        .unwrap();
        // 用户登录键(真实形态)+ aiSettings 原值(带用户数据段)
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
            rusqlite::params!["cursorAuth/accessToken", "sk-user-access-token"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
            rusqlite::params!["cursorAuth/cachedEmail", "user@example.com"],
        )
        .unwrap();
        let ai = json!({
            "globalState": { "theme": "dark", "userSettings": { "fontSize": 14 } },
            "aiSettings": { "model": "gpt-4o-mini", "openAIBaseUrl": "https://api.openai.com/v1", "useOpenAIKey": false },
            "misc": { "onboardingDone": true }
        });
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
            rusqlite::params![AI_SETTINGS_KEY, ai.to_string()],
        )
        .unwrap();
        drop(conn);
        db
    }

    fn provider_json(path: &Path, base: &str) {
        let body = json!({
            "version": 1,
            "active_provider_id": null,
            "providers": [{
                "id": "cur-p", "name": "测试站", "agent": "cursor",
                "base_url": base, "api_key": "sk-test-key", "model": "gpt-test",
                "wire_api": "chat_completions"
            }]
        });
        std::fs::write(path, serde_json::to_string_pretty(&body).unwrap()).unwrap();
    }

    fn load_provider_json(db: &Path) -> Value {
        read_ai_settings(db).unwrap()
    }

    #[test]
    fn host_writes_only_three_places_and_preserves_user_data() {
        let root = std::env::temp_dir().join(format!("2xapi-cursor-host-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let db = spec_db(&root);
        let bk = root.join("bk");
        let pv = root.join("providers.json");
        provider_json(&pv, "https://2xa.cc.cd");

        let r = host(&root, &bk, &pv, "cur-p", "gateway").unwrap();
        assert_eq!(r["hosted"], true);
        assert_eq!(r["way"], "gateway");
        assert!(r["baseUrl"].as_str().unwrap().contains("127.0.0.1:8787"));

        // ① aiSettings 两字段
        let ai = load_provider_json(&db);
        assert_eq!(ai["aiSettings"]["useOpenAIKey"], true);
        assert_eq!(
            ai["aiSettings"]["openAIBaseUrl"],
            "http://127.0.0.1:8787/v1"
        );
        // ② 用户数据段零触碰
        assert_eq!(ai["globalState"]["theme"], "dark");
        assert_eq!(ai["misc"]["onboardingDone"], true);
        assert_eq!(ai["aiSettings"]["model"], "gpt-4o-mini");
        // ③ Key 明文键 + ④ 用户登录键零触碰
        assert_eq!(
            read_item(&db, OPENAI_KEY_KEY).unwrap().as_deref(),
            Some("2xapi-gateway-managed")
        );
        assert_eq!(
            read_item(&db, "cursorAuth/accessToken").unwrap().as_deref(),
            Some("sk-user-access-token")
        );
        // ⑤ 快照与备份在
        assert!(snapshot_path(&bk).exists());
        assert!(
            std::fs::read_dir(&bk).unwrap().any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("cursor-vscdb-")),
            "vscdb 整体备份应在"
        );
        // ⑥ set_active
        let data = crate::providers::load(&pv);
        assert_eq!(data.active_provider_id.as_deref(), Some("cur-p"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn host_direct_writes_real_key_with_v1_rule() {
        let root = std::env::temp_dir().join(format!("2xapi-cursor-direct-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let db = spec_db(&root);
        let bk = root.join("bk");
        let pv = root.join("providers.json");
        provider_json(&pv, "https://2xa.cc.cd");

        host(&root, &bk, &pv, "cur-p", "direct").unwrap();
        let ai = load_provider_json(&db);
        assert_eq!(ai["aiSettings"]["openAIBaseUrl"], "https://2xa.cc.cd/v1");
        assert_eq!(
            read_item(&db, OPENAI_KEY_KEY).unwrap().as_deref(),
            Some("sk-test-key")
        );
        // 裸域带 /v1 尾不双叠
        provider_json(&pv, "https://2xa.cc.cd/v1");
        let root2 = root.join("h2");
        std::fs::create_dir_all(&root2).unwrap();
        let db2 = spec_db(&root2);
        host(&root2, &root2.join("bk"), &pv, "cur-p", "direct").unwrap();
        assert_eq!(
            load_provider_json(&db2)["aiSettings"]["openAIBaseUrl"],
            "https://2xa.cc.cd/v1"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn host_idempotent_and_unhost_restores_exactly() {
        let root = std::env::temp_dir().join(format!("2xapi-cursor-cycle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let db = spec_db(&root);
        let bk = root.join("bk");
        let pv = root.join("providers.json");
        provider_json(&pv, "https://2xa.cc.cd");
        let original = load_provider_json(&db);

        host(&root, &bk, &pv, "cur-p", "gateway").unwrap();
        let after_host = load_provider_json(&db);
        // 幂等:二次 host 值不变
        host(&root, &bk, &pv, "cur-p", "gateway").unwrap();
        assert_eq!(load_provider_json(&db), after_host);

        // unhost → 快照精确还原(语义等价)
        let r = unhost(&root, &bk).unwrap();
        assert_eq!(r["changed"], true);
        assert_eq!(load_provider_json(&db), original);
        assert!(read_item(&db, OPENAI_KEY_KEY).unwrap().is_none());
        // 用户登录键仍在
        assert_eq!(
            read_item(&db, "cursorAuth/accessToken").unwrap().as_deref(),
            Some("sk-user-access-token")
        );

        // 再 unhost = no-op
        let r2 = unhost(&root, &bk).unwrap();
        assert_eq!(r2["changed"], false);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unhost_without_snapshot_strips_controlled_fields() {
        let root = std::env::temp_dir().join(format!("2xapi-cursor-nosnap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let db = spec_db(&root);
        let bk = root.join("bk");
        // 直接写受控字段(模拟历史/无快照场景)
        let mut ai = load_provider_json(&db);
        let s = ai_settings_mut(&mut ai).unwrap();
        s.insert("useOpenAIKey".into(), json!(true));
        s.insert("openAIBaseUrl".into(), json!("http://127.0.0.1:8787/v1"));
        write_ai_settings(&db, &ai).unwrap();
        upsert_item(&db, OPENAI_KEY_KEY, "2xapi-gateway-managed").unwrap();

        unhost(&root, &bk).unwrap();
        let after = load_provider_json(&db);
        assert!(after["aiSettings"].get("useOpenAIKey").is_none());
        assert!(after["aiSettings"].get("openAIBaseUrl").is_none());
        assert!(read_item(&db, OPENAI_KEY_KEY).unwrap().is_none());
        assert_eq!(after["globalState"]["theme"], "dark");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unhost_preserves_later_unrelated_changes_and_restores_original_key() {
        let root =
            std::env::temp_dir().join(format!("2xapi-cursor-surgical-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let db = spec_db(&root);
        upsert_item(&db, OPENAI_KEY_KEY, "user-original-key").unwrap();
        let backup = root.join("bk");
        let providers = root.join("providers.json");
        provider_json(&providers, "https://2xa.cc.cd");

        host(&root, &backup, &providers, "cur-p", "gateway").unwrap();
        let mut changed = read_ai_settings(&db).unwrap();
        changed["globalState"]["theme"] = json!("light");
        write_ai_settings(&db, &changed).unwrap();

        unhost(&root, &backup).unwrap();
        let restored = read_ai_settings(&db).unwrap();
        assert_eq!(restored["globalState"]["theme"], "light");
        assert_eq!(
            restored["aiSettings"]["openAIBaseUrl"],
            "https://api.openai.com/v1"
        );
        assert_eq!(
            read_item(&db, OPENAI_KEY_KEY).unwrap().as_deref(),
            Some("user-original-key")
        );
        assert!(
            crate::providers::get_active_for_agent(&providers, "cursor").is_none(),
            "unhost 必须清理 Cursor active"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn state_shape_and_hosted_detection() {
        let root = std::env::temp_dir().join(format!("2xapi-cursor-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // 未安装
        let s0 = state(&root);
        assert_eq!(s0["installed"], false);
        assert!(s0["hosting"].is_null());
        // 安装未托管
        spec_db(&root);
        let s1 = state(&root);
        assert_eq!(s1["installed"], true);
        assert!(s1["hosting"].is_null());
        // 托管中
        let bk = root.join("bk");
        let pv = root.join("providers.json");
        provider_json(&pv, "https://2xa.cc.cd");
        host(&root, &bk, &pv, "cur-p", "gateway").unwrap();
        let s2 = state(&root);
        assert_eq!(s2["hosting"]["way"], "gateway");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn host_missing_vscdb_404_and_bad_way_400() {
        let root = std::env::temp_dir().join(format!("2xapi-cursor-err-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let pv = root.join("providers.json");
        provider_json(&pv, "https://2xa.cc.cd");
        let err = host(&root, &root.join("bk"), &pv, "cur-p", "gateway").unwrap_err();
        assert_eq!(err.0, 404);
        assert_eq!(err.1, "E_CURSOR_NOT_FOUND");
        spec_db(&root);
        let err2 = host(&root, &root.join("bk"), &pv, "cur-p", "sideways").unwrap_err();
        assert_eq!(err2.0, 400);
        assert_eq!(err2.1, "E_BAD_WAY");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn host_rejects_foreign_agent_without_db_changes() {
        let root =
            std::env::temp_dir().join(format!("2xapi-cursor-foreign-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let db = spec_db(&root);
        let providers = root.join("providers.json");
        provider_json(&providers, "https://2xa.cc.cd");
        let mut data: Value =
            serde_json::from_str(&std::fs::read_to_string(&providers).unwrap()).unwrap();
        data["providers"][0]["agent"] = json!("codex");
        std::fs::write(&providers, data.to_string()).unwrap();
        let before = read_ai_settings(&db).unwrap();

        let error = host(&root, &root.join("bk"), &providers, "cur-p", "gateway").unwrap_err();

        assert_eq!(error.1, "E_PROVIDER_AGENT_MISMATCH");
        assert_eq!(read_ai_settings(&db).unwrap(), before);
        assert!(read_item(&db, MANAGED_KEY).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn snapshot_file_roundtrip() {
        // 快照写读往返(防路径拼接错)
        let root = std::env::temp_dir().join(format!("2xapi-cursor-snap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let bk = root.join("bk/deep");
        let snap = snapshot_path(&bk);
        std::fs::create_dir_all(snap.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(&snap).unwrap();
        f.write_all(br#"{"aiSettings":{"x":1}}"#).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&std::fs::read_to_string(&snap).unwrap()).unwrap()
                ["aiSettings"]["x"],
            1
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
