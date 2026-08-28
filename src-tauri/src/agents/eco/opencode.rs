//! OpenCode 生态 adapter:`~/.config/opencode/opencode.json` 的 `mcp` 段。
//! 条目形状与通用 stdio spec 不同(cc-switch mcp/opencode.rs 转换规则实证):
//! 写入 `{type:"local", command:[cmd, ...args], environment:{}, enabled:true}`;
//! 读侧归一为 `{command, args, env}`(command 数组首元素提为 command);remote 型只读展示。

use super::EcoStore;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct OpencodeStore {
    path: PathBuf,
}

impl OpencodeStore {
    pub fn new(oc_home: &Path) -> Self {
        Self {
            path: oc_home
                .join(".config")
                .join("opencode")
                .join("opencode.json"),
        }
    }

    fn read_doc(&self) -> Result<Map<String, Value>, super::OpError> {
        if !self.path.exists() {
            return Ok(Map::new());
        }
        let raw = std::fs::read_to_string(&self.path).map_err(|e| {
            (
                500,
                "E_IO".to_string(),
                format!("读取 opencode.json 失败: {e}"),
            )
        })?;
        // 容忍 jsonc 尾注释/尾逗号:剥 // 行注释再 parse(cc-switch opencode_config 同款口径)
        let cleaned: String = raw
            .lines()
            .map(|l| {
                let t = l.trim_start();
                if t.starts_with("//") {
                    ""
                } else {
                    l
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        serde_json::from_str::<Value>(&cleaned)
            .map_err(|_| {
                (
                    500,
                    "E_PARSE".to_string(),
                    "opencode.json 不是合法 JSON,已拒绝写入(避免破坏手动配置);请先修复该文件"
                        .to_string(),
                )
            })?
            .as_object()
            .cloned()
            .ok_or_else(|| {
                (
                    500,
                    "E_PARSE".to_string(),
                    "opencode.json 顶层必须是对象".to_string(),
                )
            })
    }

    fn write_doc(&self, doc: &Map<String, Value>) -> Result<(), super::OpError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                (
                    500,
                    "E_IO".to_string(),
                    format!("创建 opencode 目录失败: {e}"),
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

/// 通用 stdio spec → opencode local 条目。
fn spec_to_entry(spec: &Value) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), json!("local"));
    let cmd = spec.get("command").and_then(|v| v.as_str()).unwrap_or("");
    let mut command = vec![json!(cmd)];
    if let Some(args) = spec.get("args").and_then(|v| v.as_array()) {
        command.extend(args.iter().cloned());
    }
    m.insert("command".into(), Value::Array(command));
    if let Some(env) = spec.get("env").and_then(|v| v.as_object()) {
        if !env.is_empty() {
            m.insert("environment".into(), Value::Object(env.clone()));
        }
    }
    m.insert("enabled".into(), json!(true));
    Value::Object(m)
}

/// opencode 条目 → 通用 spec(command 数组拆首元素)。
fn entry_to_spec(entry: &Value) -> Value {
    let typ = entry
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("local");
    if typ == "remote" {
        return entry.clone(); // remote 只读展示,summary 走 url
    }
    let mut m = Map::new();
    if let Some(arr) = entry.get("command").and_then(|v| v.as_array()) {
        if let Some(first) = arr.first().and_then(|v| v.as_str()) {
            m.insert("command".into(), json!(first));
        }
        if arr.len() > 1 {
            m.insert("args".into(), Value::Array(arr[1..].to_vec()));
        }
    } else if let Some(cmd) = entry.get("command").and_then(|v| v.as_str()) {
        m.insert("command".into(), json!(cmd));
    }
    if let Some(env) = entry
        .get("environment")
        .or_else(|| entry.get("env"))
        .and_then(|v| v.as_object())
    {
        m.insert("env".into(), Value::Object(env.clone()));
    }
    if m.is_empty() {
        return entry.clone();
    }
    Value::Object(m)
}

impl EcoStore for OpencodeStore {
    fn id(&self) -> &'static str {
        "opencode"
    }

    fn read(&self) -> Result<BTreeMap<String, Value>, super::OpError> {
        let doc = self.read_doc()?;
        let mut out = BTreeMap::new();
        if let Some(servers) = doc.get("mcp").and_then(|v| v.as_object()) {
            for (k, v) in servers {
                out.insert(k.clone(), entry_to_spec(v));
            }
        }
        Ok(out)
    }

    fn write(&self, servers: &BTreeMap<String, Value>) -> Result<(), super::OpError> {
        let mut doc = self.read_doc()?;
        let existing = doc
            .get("mcp")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if servers.is_empty() {
            doc.remove("mcp");
        } else {
            let mut m = Map::new();
            for (k, v) in servers {
                let entry = existing
                    .get(k)
                    .filter(|raw| entry_to_spec(raw) == *v)
                    .cloned()
                    .unwrap_or_else(|| spec_to_entry(v));
                m.insert(k.clone(), entry);
            }
            doc.insert("mcp".into(), Value::Object(m));
        }
        self.write_doc(&doc)
    }

    fn backup(&self, backup_dir: &Path) -> Result<(), super::OpError> {
        crate::config::backup_file(&self.path, backup_dir, "eco-apply", "pre-eco")
            .map(|_| ())
            .map_err(|e| (500, "E_IO".to_string(), e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("2xapi-eco-oc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn roundtrip_and_conversion_shape() {
        let root = root("roundtrip");
        let s = OpencodeStore::new(&root);
        let mut m = BTreeMap::new();
        m.insert(
            "fetch".to_string(),
            json!({ "command": "uvx", "args": ["mcp-server-fetch"], "env": { "K": "v" } }),
        );
        s.write(&m).unwrap();
        let raw: Value = serde_json::from_str(
            &std::fs::read_to_string(root.join(".config/opencode/opencode.json")).unwrap(),
        )
        .unwrap();
        let entry = &raw["mcp"]["fetch"];
        assert_eq!(entry["type"], "local");
        assert_eq!(entry["command"][0], "uvx");
        assert_eq!(entry["command"][1], "mcp-server-fetch");
        assert_eq!(entry["environment"]["K"], "v");
        assert_eq!(entry["enabled"], true);
        // 读回归一形状
        let back = s.read().unwrap();
        assert_eq!(back["fetch"]["command"], "uvx");
        assert_eq!(back["fetch"]["args"][0], "mcp-server-fetch");
    }

    #[test]
    fn jsonc_comments_tolerated_and_other_keys_preserved() {
        let root = root("jsonc");
        let path = root.join(".config/opencode/opencode.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "{\n  // 用户注释\n  \"theme\": \"dark\",\n  \"mcp\": {}\n}",
        )
        .unwrap();
        let s = OpencodeStore::new(&root);
        let mut m = BTreeMap::new();
        m.insert("a".to_string(), json!({ "command": "x" }));
        s.write(&m).unwrap();
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["theme"], "dark", "其他键保留");
        assert_eq!(doc["mcp"]["a"]["type"], "local");
    }

    #[test]
    fn empty_write_removes_section() {
        let root = root("empty");
        let s = OpencodeStore::new(&root);
        let mut m = BTreeMap::new();
        m.insert("a".to_string(), json!({ "command": "x" }));
        s.write(&m).unwrap();
        s.write(&BTreeMap::new()).unwrap();
        let doc: Value = serde_json::from_str(
            &std::fs::read_to_string(root.join(".config/opencode/opencode.json")).unwrap(),
        )
        .unwrap();
        assert!(doc.get("mcp").is_none());
    }

    #[test]
    fn unchanged_manual_entries_keep_native_shape_and_unknown_fields() {
        let root = root("preserve-manual");
        let path = root.join(".config/opencode/opencode.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "mcp": {
                    "remote-user": {"type":"remote","url":"https://mcp.example","enabled":false,"x-user":1},
                    "local-user": {"type":"local","command":["node","server.js"],"enabled":false,"x-user":"keep"}
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let store = OpencodeStore::new(&root);
        let mut servers = store.read().unwrap();
        servers.insert(
            "console-new".into(),
            json!({"command":"npx","args":["mcp-x"]}),
        );
        store.write(&servers).unwrap();

        let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(doc["mcp"]["remote-user"]["type"], "remote");
        assert_eq!(doc["mcp"]["remote-user"]["enabled"], false);
        assert_eq!(doc["mcp"]["remote-user"]["x-user"], 1);
        assert_eq!(doc["mcp"]["local-user"]["enabled"], false);
        assert_eq!(doc["mcp"]["local-user"]["x-user"], "keep");
        assert_eq!(doc["mcp"]["console-new"]["type"], "local");
    }
}
