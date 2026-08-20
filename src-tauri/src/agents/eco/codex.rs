//! Codex 生态 adapter:`~/.codex/config.toml` 的 `[mcp_servers.*]` 段。
//! 与托管引擎(desktop.rs)同一文件、互不干扰:托管动 custom/model 等键,
//! 本 adapter 只动 mcp_servers 键(toml 直接 parse→改→render,不走 serde_json
//! 往返,保 grok_config 段级手法);用户已有的 mcp_servers 条目属于手动条目,
//! 由 mod.rs 登记表拦截,零触碰。

use super::EcoStore;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct TomlStore {
    id: &'static str,
    path: PathBuf,
}

impl TomlStore {
    pub fn new(config_path: &Path) -> Self {
        Self {
            id: "codex",
            path: config_path.to_path_buf(),
        }
    }

    /// 通用构造:B 段 grok 复用(~/.grok/config.toml [mcp_servers] 同段名)。
    pub fn at(id: &'static str, config_path: &Path) -> Self {
        Self {
            id,
            path: config_path.to_path_buf(),
        }
    }

    fn parse_root(&self) -> Result<toml::value::Table, super::OpError> {
        if !self.path.exists() {
            return Ok(toml::value::Table::new());
        }
        let raw = std::fs::read_to_string(&self.path).map_err(|e| {
            (
                500,
                "E_IO".to_string(),
                format!("读取 config.toml 失败: {e}"),
            )
        })?;
        raw.parse::<toml::Value>()
            .map_err(|_| {
                (
                    500,
                    "E_PARSE".to_string(),
                    "config.toml 不是合法 TOML,已拒绝写入(避免破坏手动配置);请先修复该文件"
                        .to_string(),
                )
            })?
            .as_table()
            .cloned()
            .ok_or_else(|| {
                (
                    500,
                    "E_PARSE".to_string(),
                    "config.toml 顶层必须是表".to_string(),
                )
            })
    }

    fn render(&self, root: &toml::value::Table) -> Result<(), super::OpError> {
        let text = toml::to_string_pretty(&toml::Value::Table(root.clone()))
            .map_err(|e| (500, "E_IO".to_string(), format!("TOML 编码失败: {e}")))?;
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, &text)
            .map_err(|e| (500, "E_IO".to_string(), format!("写入临时文件失败: {e}")))?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| (500, "E_IO".to_string(), format!("原子替换失败: {e}")))
    }

    fn toml_to_json(v: &toml::Value) -> Value {
        match v {
            toml::Value::String(s) => Value::String(s.clone()),
            toml::Value::Integer(i) => serde_json::json!(i),
            toml::Value::Boolean(b) => Value::Bool(*b),
            toml::Value::Array(a) => Value::Array(a.iter().map(Self::toml_to_json).collect()),
            toml::Value::Table(t) => {
                let mut m = serde_json::Map::new();
                for (k, v) in t {
                    m.insert(k.clone(), Self::toml_to_json(v));
                }
                Value::Object(m)
            }
            toml::Value::Float(f) => serde_json::json!(f),
            toml::Value::Datetime(d) => Value::String(d.to_string()),
        }
    }

    fn json_to_toml(v: &Value) -> Result<toml::Value, String> {
        match v {
            Value::String(s) => Ok(toml::Value::String(s.clone())),
            Value::Bool(b) => Ok(toml::Value::Boolean(*b)),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Ok(toml::Value::Integer(i))
                } else {
                    n.as_f64()
                        .map(toml::Value::Float)
                        .ok_or_else(|| "不支持的数字".to_string())
                }
            }
            Value::Array(a) => {
                let mut out = Vec::new();
                for x in a {
                    out.push(Self::json_to_toml(x)?);
                }
                Ok(toml::Value::Array(out))
            }
            Value::Object(m) => {
                let mut t = toml::value::Table::new();
                for (k, x) in m {
                    t.insert(k.clone(), Self::json_to_toml(x)?);
                }
                Ok(toml::Value::Table(t))
            }
            Value::Null => Err("TOML 不支持 null 值".to_string()),
        }
    }
}

impl EcoStore for TomlStore {
    fn id(&self) -> &'static str {
        self.id
    }

    fn read(&self) -> Result<BTreeMap<String, Value>, super::OpError> {
        let root = self.parse_root()?;
        let mut out = BTreeMap::new();
        if let Some(toml::Value::Table(servers)) = root.get("mcp_servers") {
            for (k, v) in servers {
                out.insert(k.clone(), Self::toml_to_json(v));
            }
        }
        Ok(out)
    }

    fn write(&self, servers: &BTreeMap<String, Value>) -> Result<(), super::OpError> {
        let mut root = self.parse_root()?;
        if servers.is_empty() {
            root.remove("mcp_servers");
        } else {
            let mut t = toml::value::Table::new();
            for (k, v) in servers {
                let tv =
                    Self::json_to_toml(v).map_err(|e| (400, "E_ECO_BAD_SPEC".to_string(), e))?;
                t.insert(k.clone(), tv);
            }
            root.insert("mcp_servers".into(), toml::Value::Table(t));
        }
        self.render(&root)
    }

    /// Codex [mcp_servers.x] 原生 enabled 布尔(侦察报告实证,本机在用)。
    fn native_enabled(&self) -> bool {
        self.id == "codex"
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
        let root = std::env::temp_dir().join(format!("2xapi-eco-cx-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn write_mcp_servers_preserves_other_sections() {
        let root = root("preserve");
        let path = root.join("config.toml");
        std::fs::write(
            &path,
            "model = \"gpt-5\"\nnotify = [\"a\"]\n[mcp_servers.computer-use]\ncommand = \"node\"\n[custom]\nbase_url = \"http://x\"\n",
        )
        .unwrap();
        let s = TomlStore::new(&path);
        assert_eq!(s.read().unwrap().len(), 1);
        // 模拟真实调用路径:read → 追加 → write(已有条目 computer-use 保留)
        let mut m = s.read().unwrap();
        m.insert(
            "fetch".to_string(),
            json!({ "command": "uvx", "args": ["mcp-server-fetch"], "env": { "K": "v" } }),
        );
        s.write(&m).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let doc = text.parse::<toml::Value>().unwrap();
        assert_eq!(doc["model"].as_str(), Some("gpt-5"), "其他键必须保留");
        assert_eq!(doc["custom"]["base_url"].as_str(), Some("http://x"));
        assert_eq!(doc["mcp_servers"]["fetch"]["command"].as_str(), Some("uvx"));
        assert_eq!(doc["mcp_servers"]["fetch"]["env"]["K"].as_str(), Some("v"));
        assert_eq!(
            doc["mcp_servers"]["computer-use"]["command"].as_str(),
            Some("node")
        );
        // 读回形状
        let back = s.read().unwrap();
        assert_eq!(back["fetch"]["args"][0], "mcp-server-fetch");
        assert_eq!(back["computer-use"]["command"], "node");
    }

    #[test]
    fn empty_write_removes_section() {
        let root = root("remove");
        let path = root.join("config.toml");
        std::fs::write(&path, "[mcp_servers.x]\ncommand = \"c\"\n").unwrap();
        let s = TomlStore::new(&path);
        s.write(&BTreeMap::new()).unwrap();
        let doc = std::fs::read_to_string(&path)
            .unwrap()
            .parse::<toml::Value>()
            .unwrap();
        assert!(doc.get("mcp_servers").is_none());
    }

    #[test]
    fn parse_failure_refuses_to_touch() {
        let root = root("parse");
        let path = root.join("config.toml");
        std::fs::write(&path, "not [ valid toml").unwrap();
        let s = TomlStore::new(&path);
        assert_eq!(s.read().unwrap_err().1, "E_PARSE");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not [ valid toml");
    }
}
