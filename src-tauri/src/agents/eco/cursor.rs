//! JSON 载体生态 adapter:B 段泛化,Cursor / TRAE / Claude Desktop 共用。
//! - cursor:`~/.cursor/mcp.json`(mcpServers 段)
//! - trae:`~/.trae/mcp.json`(mcpServers 段;E1 定案,与 Cursor 同构,
//!   docs.trae.ai 官方)
//! - claude-desktop:`claude_desktop_config.json`(mcpServers 段;文件含用户
//!   其他键,只动本段)
//!
//! 读:不存在 → 空;parse 失败 → E_PARSE 拒碰(workbuddy 先例)。
//! 写:读→改段→pretty JSON 原子写,其余顶层键保留。

use super::EcoStore;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct JsonStore {
    id: &'static str,
    path: PathBuf,
    /// MCP 条目所在键路径:单层=[mcpServers](cursor/trae/claude-desktop/workbuddy/claude);
    /// 嵌套=[mcp,servers](openclaw)。
    sections: &'static [&'static str],
    /// 平台条目是否原生支持 enabled 停用字段(openclaw 实证 true,同 Codex;其余 JSON 平台无)。
    native_enabled: bool,
}

impl JsonStore {
    pub fn new(cursor_home: &Path) -> Self {
        Self::at("cursor", &cursor_home.join(".cursor").join("mcp.json"))
    }

    /// 通用构造(段名默认 mcpServers,无原生 enabled)。
    pub fn at(id: &'static str, path: &Path) -> Self {
        Self::nested(id, path, &["mcpServers"], false)
    }

    /// 嵌套段构造(openclaw: mcp.servers)。
    pub fn nested(
        id: &'static str,
        path: &Path,
        sections: &'static [&'static str],
        native_enabled: bool,
    ) -> Self {
        Self {
            id,
            path: path.to_path_buf(),
            sections,
            native_enabled,
        }
    }

    fn read_doc(&self) -> Result<serde_json::Map<String, Value>, super::OpError> {
        if !self.path.exists() {
            return Ok(serde_json::Map::new());
        }
        let raw = std::fs::read_to_string(&self.path)
            .map_err(|e| (500, "E_IO".to_string(), format!("读取 mcp.json 失败: {e}")))?;
        serde_json::from_str::<Value>(&raw)
            .map_err(|_| {
                (
                    500,
                    "E_PARSE".to_string(),
                    "mcp.json 不是合法 JSON,已拒绝写入(避免破坏手动配置);请先修复该文件"
                        .to_string(),
                )
            })?
            .as_object()
            .cloned()
            .ok_or_else(|| {
                (
                    500,
                    "E_PARSE".to_string(),
                    "mcp.json 顶层必须是对象".to_string(),
                )
            })
    }

    fn write_doc(&self, doc: &serde_json::Map<String, Value>) -> Result<(), super::OpError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                (
                    500,
                    "E_IO".to_string(),
                    format!("创建 {} 目录失败: {e}", self.path.display()),
                )
            })?;
        }
        let text = serde_json::to_string_pretty(&Value::Object(doc.clone()))
            .map_err(|e| (500, "E_IO".to_string(), format!("JSON 编码失败: {e}")))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, format!("{text}\n"))
            .map_err(|e| (500, "E_IO".to_string(), format!("写入临时文件失败: {e}")))?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| (500, "E_IO".to_string(), format!("原子替换失败: {e}")))
    }
}

impl EcoStore for JsonStore {
    fn id(&self) -> &'static str {
        self.id
    }

    fn native_enabled(&self) -> bool {
        self.native_enabled
    }

    fn read(&self) -> Result<BTreeMap<String, Value>, super::OpError> {
        let doc = self.read_doc()?;
        let mut node = Value::Object(doc);
        for k in self.sections {
            node = node.get(*k).cloned().unwrap_or(Value::Null);
        }
        let mut out = BTreeMap::new();
        if let Some(servers) = node.as_object() {
            for (k, v) in servers {
                out.insert(k.clone(), v.clone());
            }
        }
        Ok(out)
    }

    fn write(&self, servers: &BTreeMap<String, Value>) -> Result<(), super::OpError> {
        let mut doc = self.read_doc()?;
        if servers.is_empty() {
            if remove_nested(&mut doc, self.sections) {
                self.write_doc(&doc)?;
            }
            return Ok(());
        }
        let mut m = serde_json::Map::new();
        for (k, v) in servers {
            m.insert(k.clone(), v.clone());
        }
        set_nested(&mut doc, self.sections, Value::Object(m));
        self.write_doc(&doc)
    }

    fn backup(&self, backup_dir: &Path) -> Result<(), super::OpError> {
        crate::config::backup_file(&self.path, backup_dir, "eco-apply", "pre-eco")
            .map(|_| ())
            .map_err(|e| (500, "E_IO".to_string(), e))
    }
}

/// 沿键路径写值:中间层缺失或非对象时覆盖为对象(openclaw: mcp.servers)。
fn set_nested(obj: &mut serde_json::Map<String, Value>, keys: &[&str], v: Value) {
    let (head, rest) = keys.split_first().expect("键路径非空");
    if rest.is_empty() {
        obj.insert(head.to_string(), v);
        return;
    }
    let entry = obj
        .entry(head.to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(serde_json::Map::new());
    }
    set_nested(entry.as_object_mut().unwrap(), rest, v);
}

/// 沿键路径删末层键;父层因此变空则连带删除(零残留,与单层段语义一致)。
fn remove_nested(obj: &mut serde_json::Map<String, Value>, keys: &[&str]) -> bool {
    let Some((head, rest)) = keys.split_first() else {
        return false;
    };
    if rest.is_empty() {
        return obj.remove(*head).is_some();
    }
    let Some(child) = obj.get_mut(*head).and_then(|v| v.as_object_mut()) else {
        return false;
    };
    let removed = remove_nested(child, rest);
    if removed && child.is_empty() {
        obj.remove(*head);
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("2xapi-eco-cur-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn read_missing_is_empty_and_write_creates() {
        let root = root("create");
        let s = JsonStore::new(&root);
        assert!(s.read().unwrap().is_empty());
        let mut m = BTreeMap::new();
        m.insert(
            "fetch".to_string(),
            json!({ "command": "uvx", "args": ["mcp-server-fetch"] }),
        );
        s.write(&m).unwrap();
        let raw = std::fs::read_to_string(root.join(".cursor/mcp.json")).unwrap();
        assert!(raw.contains("\"mcpServers\""));
        assert!(raw.contains("mcp-server-fetch"));
        assert_eq!(s.read().unwrap()["fetch"]["command"], "uvx");
    }

    #[test]
    fn write_preserves_other_top_level_keys() {
        let root = root("preserve");
        let path = root.join(".cursor/mcp.json");
        std::fs::create_dir_all(root.join(".cursor")).unwrap();
        std::fs::write(
            &path,
            r#"{ "other": 1, "mcpServers": { "old": { "command": "x" } } }"#,
        )
        .unwrap();
        let s = JsonStore::new(&root);
        let mut m = BTreeMap::new();
        m.insert("new".to_string(), json!({ "command": "y" }));
        s.write(&m).unwrap();
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["other"], 1, "其余顶层键必须保留");
        assert!(doc["mcpServers"].get("old").is_none());
        assert!(doc["mcpServers"]["new"].is_object());
    }

    #[test]
    fn parse_failure_refuses_to_touch() {
        let root = root("parse");
        let path = root.join(".cursor/mcp.json");
        std::fs::create_dir_all(root.join(".cursor")).unwrap();
        std::fs::write(&path, "{ broken").unwrap();
        let s = JsonStore::new(&root);
        let err = s.read().unwrap_err();
        assert_eq!(err.1, "E_PARSE");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ broken",
            "坏文件必须原样保留"
        );
    }

    #[test]
    fn write_empty_removes_section() {
        let root = root("empty");
        let s = JsonStore::new(&root);
        let mut m = BTreeMap::new();
        m.insert("a".to_string(), json!({ "command": "x" }));
        s.write(&m).unwrap();
        s.write(&BTreeMap::new()).unwrap();
        let doc: Value =
            serde_json::from_str(&std::fs::read_to_string(root.join(".cursor/mcp.json")).unwrap())
                .unwrap();
        assert!(doc.get("mcpServers").is_none());
    }

    #[test]
    fn backup_leaves_snapshot() {
        let root = root("backup");
        let bk = root.join("backups");
        std::fs::create_dir_all(&bk).unwrap();
        let s = JsonStore::new(&root);
        let mut m = BTreeMap::new();
        m.insert("a".to_string(), json!({ "command": "x" }));
        s.write(&m).unwrap();
        s.backup(&bk).unwrap();
        assert!(
            std::fs::read_dir(&bk).unwrap().count() >= 1,
            "备份目录应有快照"
        );
    }

    /// openclaw 形状(2026-08-17 CLI 隔离 HOME 实证:`openclaw mcp add` 落盘
    /// mcp.servers.<name>={command,args,env?,enabled?},enabled:false=原生停用,无键=启用;
    /// 同文档并存 commands/messages/agents/cron/meta 等其他顶层键)。
    fn openclaw_doc() -> &'static str {
        r#"{
  "commands": { "native": "auto", "restart": true },
  "mcp": {
    "servers": {
      "probe-x": { "command": "npx", "args": ["hello-server"] }
    }
  },
  "messages": { "ackReactionScope": "group-mentions" },
  "meta": { "lastTouchedVersion": "2026.7.1-2" }
}"#
    }

    #[test]
    fn nested_read_write_preserves_other_keys() {
        let root = root("openclaw");
        let dir = root.join(".openclaw");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("openclaw.json");
        std::fs::write(&path, openclaw_doc()).unwrap();
        let s = JsonStore::nested("openclaw", &path, &["mcp", "servers"], true);
        assert!(
            s.native_enabled(),
            "openclaw 原生 enabled(实证 enabled:false)"
        );

        let mut before = s.read().unwrap();
        assert_eq!(before.keys().collect::<Vec<_>>(), vec!["probe-x"]);
        // 写入 Console 条目:已有 probe-x 与 mcp 外其他键零变化
        before.insert(
            "memory".to_string(),
            json!({ "command": "npx", "args": ["-y", "server-memory"] }),
        );
        s.write(&before).unwrap();
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["mcp"]["servers"]["probe-x"]["args"][0], "hello-server");
        assert_eq!(doc["mcp"]["servers"]["memory"]["command"], "npx");
        assert_eq!(doc["commands"]["restart"], true, "其他顶层键保留");
        assert_eq!(doc["meta"]["lastTouchedVersion"], "2026.7.1-2");

        // 清空:mcp 段整体移除(零残留),其他顶层键保留
        s.write(&BTreeMap::new()).unwrap();
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(doc.get("mcp").is_none(), "mcp 空应连带移除");
        assert!(doc.get("commands").is_some());
        assert!(s.read().unwrap().is_empty());
    }

    #[test]
    fn nested_write_creates_missing_intermediates() {
        let root = root("openclaw-new");
        let path = root.join(".openclaw/openclaw.json");
        let s = JsonStore::nested("openclaw", &path, &["mcp", "servers"], true);
        assert!(s.read().unwrap().is_empty(), "文件不存在读为空");
        let mut m = BTreeMap::new();
        m.insert("a".to_string(), json!({ "command": "x" }));
        s.write(&m).unwrap();
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["mcp"]["servers"]["a"]["command"], "x", "中间层自动创建");
    }
}
