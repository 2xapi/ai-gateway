//! Hermes 生态 adapter:`~/.hermes/config.yaml` 的 `mcp_servers` 段。
//! YAML 段级替换(手法复刻 hermes.rs 自包含版,避免动其私有函数):
//! 读侧治愈重复顶层键 → serde_yaml 解析;写侧 find_section_range 原位替换 `mcp_servers`
//! 段(其余段文本原样保留),未命中则追加文件尾;原子写。
//! 本机实证 config.yaml 有 `mcp_servers: {}` 空段(21 项技能另在 skills 目录,属 C 段)。

use super::EcoStore;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct HermesStore {
    path: PathBuf,
}

impl HermesStore {
    pub fn new(hermes_home: &Path) -> Self {
        Self {
            path: hermes_home.join("config.yaml"),
        }
    }
}

/// 在文本中找顶层键 `key:` 的段落范围 (start, end)。终点 = 下一个**任意**顶层段行的
/// 行首(hermes.rs 原版口径:段范围互不吞并;若只看同名段行,末段会吞掉其后的其他段)。
fn find_section_range(raw: &str, key: &str) -> Option<(usize, usize)> {
    let key_pat = format!("{key}:");
    let mut all_sections: Vec<(usize, usize)> = Vec::new(); // (行首, 该段范围终点)
    let mut offset = 0usize;
    for line in raw.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let is_top = line.len() == trimmed.len();
        if is_top
            && !trimmed.starts_with('#')
            && !trimmed.starts_with("- ")
            && trimmed.contains(':')
        {
            let k = trimmed.split(':').next().unwrap_or("").trim();
            if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                all_sections.push((offset, 0));
            }
        }
        offset += line.len();
    }
    // 每段终点 = 下一段行首(末段 = 文件尾)
    for i in 0..all_sections.len() {
        all_sections[i].1 = all_sections
            .get(i + 1)
            .map(|&(s, _)| s)
            .unwrap_or(raw.len());
    }
    let (idx, &(start, _)) = all_sections.iter().enumerate().find(|&(_, &(s, _))| {
        let line_end = raw[s..].find('\n').map(|e| s + e + 1).unwrap_or(raw.len());
        let line = &raw[s..line_end];
        let t = line.trim_start();
        !t.starts_with('#') && t.starts_with(&key_pat) && line.len() - t.len() == 0
    })?;
    Some((start, all_sections[idx].1))
}

/// 读侧治愈:同名顶层段保留最后一份(hermes.rs 同款口径,防历史重复键;手法复刻自包含)。
fn deduplicate_top_level_keys(raw: &str) -> String {
    // 收集所有顶层段行 (行首偏移, key):无缩进、非注释、非列表项、含冒号
    let mut key_lines: Vec<(usize, String)> = Vec::new();
    let mut offset = 0usize;
    for line in raw.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let is_top = line.len() == trimmed.len();
        if is_top
            && !trimmed.starts_with('#')
            && !trimmed.starts_with("- ")
            && trimmed.contains(':')
        {
            let key = trimmed.split(':').next().unwrap_or("").trim().to_string();
            if !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                key_lines.push((offset, key));
            }
        }
        offset += line.len();
    }
    let mut result = String::with_capacity(raw.len());
    // 头部文本(首个段行之前,通常为注释/空白)保留
    if let Some(&(first, _)) = key_lines.first() {
        result.push_str(&raw[..first]);
    } else {
        return raw.to_string();
    }
    for (i, &(start, ref key)) in key_lines.iter().enumerate() {
        // 后面还有同名段 → 丢弃这份旧的(重复键治愈,keep-last)
        if key_lines[i + 1..].iter().any(|(_, k)| k == key) {
            continue;
        }
        // 本段终点 = 下一段行起点(段与段之间的尾随空行随本段保留)
        let end = key_lines.get(i + 1).map(|&(s, _)| s).unwrap_or(raw.len());
        result.push_str(&raw[start..end]);
    }
    result
}

fn read_yaml(path: &Path) -> Result<serde_yaml::Value, super::OpError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(serde_yaml::Value::Mapping(Default::default()))
        }
        Err(e) => {
            return Err((
                500,
                "E_IO".to_string(),
                format!("读取 config.yaml 失败: {e}"),
            ))
        }
    };
    if raw.trim().is_empty() {
        return Ok(serde_yaml::Value::Mapping(Default::default()));
    }
    let healed = deduplicate_top_level_keys(&raw);
    serde_yaml::from_str(&healed).map_err(|_| {
        (
            500,
            "E_PARSE".to_string(),
            "config.yaml 不是合法 YAML,已拒绝写入(避免破坏手动配置);请先修复该文件".to_string(),
        )
    })
}

fn yaml_to_json(v: &serde_yaml::Value) -> Value {
    match v {
        serde_yaml::Value::Null => Value::Null,
        serde_yaml::Value::Bool(b) => Value::Bool(*b),
        serde_yaml::Value::Number(n) => serde_json::from_str(&n.to_string()).unwrap_or(Value::Null),
        serde_yaml::Value::String(s) => Value::String(s.clone()),
        serde_yaml::Value::Sequence(s) => Value::Array(s.iter().map(yaml_to_json).collect()),
        serde_yaml::Value::Mapping(m) => {
            let mut out = serde_json::Map::new();
            for (k, val) in m {
                let key = match k {
                    serde_yaml::Value::String(s) => s.clone(),
                    serde_yaml::Value::Number(n) => n.to_string(),
                    serde_yaml::Value::Bool(b) => b.to_string(),
                    _ => continue, // 复合 key 不出现在本场景
                };
                out.insert(key, yaml_to_json(val));
            }
            Value::Object(out)
        }
        serde_yaml::Value::Tagged(t) => yaml_to_json(&t.value),
    }
}

fn json_to_yaml(v: &Value) -> serde_yaml::Value {
    match v {
        Value::Null => serde_yaml::Value::Null,
        Value::Bool(b) => serde_yaml::Value::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                serde_yaml::Value::Number(i.into())
            } else {
                serde_yaml::Value::Number(n.as_f64().unwrap_or(0.0).into())
            }
        }
        Value::String(s) => serde_yaml::Value::String(s.clone()),
        Value::Array(a) => serde_yaml::Value::Sequence(a.iter().map(json_to_yaml).collect()),
        Value::Object(m) => {
            let mut out = serde_yaml::Mapping::new();
            for (k, val) in m {
                out.insert(serde_yaml::Value::String(k.clone()), json_to_yaml(val));
            }
            serde_yaml::Value::Mapping(out)
        }
    }
}

/// `mcp_servers: <mapping>` 序列化为一段 YAML 文本。
fn section_to_yaml(mapping: &serde_yaml::Value) -> Result<String, String> {
    let mut wrap = serde_yaml::Mapping::new();
    wrap.insert(
        serde_yaml::Value::String("mcp_servers".to_string()),
        mapping.clone(),
    );
    serde_yaml::to_string(&serde_yaml::Value::Mapping(wrap))
        .map_err(|e| format!("YAML 段序列化失败(mcp_servers): {e}"))
}

fn remove_all_sections(raw: &str, key: &str) -> String {
    let mut result = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some((start, end)) = find_section_range(rest, key) {
        result.push_str(&rest[..start]);
        rest = &rest[end..];
    }
    result.push_str(rest);
    result
}

fn replace_section(raw: &str, value: &serde_yaml::Value) -> Result<String, String> {
    let serialized = section_to_yaml(value)?;
    Ok(match find_section_range(raw, "mcp_servers") {
        Some((start, end)) => {
            let mut out = String::with_capacity(raw.len());
            out.push_str(&raw[..start]);
            out.push_str(&serialized);
            let remainder = remove_all_sections(&raw[end..], "mcp_servers");
            if !serialized.ends_with('\n') && !remainder.is_empty() && !remainder.starts_with('\n')
            {
                out.push('\n');
            }
            out.push_str(&remainder);
            out
        }
        None => {
            let mut out = raw.to_string();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&serialized);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out
        }
    })
}

impl EcoStore for HermesStore {
    fn id(&self) -> &'static str {
        "hermes"
    }

    fn read(&self) -> Result<BTreeMap<String, Value>, super::OpError> {
        let doc = read_yaml(&self.path)?;
        let mut out = BTreeMap::new();
        if let Some(servers) = doc.get("mcp_servers").and_then(|v| v.as_mapping()) {
            for (k, v) in servers {
                if let serde_yaml::Value::String(name) = k {
                    out.insert(name.clone(), yaml_to_json(v));
                }
            }
        }
        Ok(out)
    }

    fn write(&self, servers: &BTreeMap<String, Value>) -> Result<(), super::OpError> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Err((
                    500,
                    "E_IO".to_string(),
                    format!("读取 config.yaml 失败: {e}"),
                ))
            }
        };
        let mut mapping = serde_yaml::Mapping::new();
        for (k, v) in servers {
            mapping.insert(serde_yaml::Value::String(k.clone()), json_to_yaml(v));
        }
        let new_raw = replace_section(&raw, &serde_yaml::Value::Mapping(mapping))
            .map_err(|e| (500, "E_IO".to_string(), e))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                (
                    500,
                    "E_IO".to_string(),
                    format!("创建 hermes 目录失败: {e}"),
                )
            })?;
        }
        let tmp = self.path.with_extension("yaml.tmp");
        std::fs::write(&tmp, &new_raw)
            .map_err(|e| (500, "E_IO".to_string(), format!("写入临时文件失败: {e}")))?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| (500, "E_IO".to_string(), format!("原子替换失败: {e}")))
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
        let root = std::env::temp_dir().join(format!("2xapi-eco-her-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    const SAMPLE: &str = "_config_version: 33\nmodel:\n  provider: openai-api\n  default: gpt-5.5\nmcp_servers: {}\nagent:\n  name: demo\n";

    #[test]
    fn write_replaces_section_preserves_others() {
        let root = root("preserve");
        let path = root.join("config.yaml");
        std::fs::write(&path, SAMPLE).unwrap();
        let s = HermesStore::new(&root);
        assert_eq!(s.read().unwrap().len(), 0);

        let mut m = BTreeMap::new();
        m.insert(
            "fetch".to_string(),
            json!({ "command": "uvx", "args": ["mcp-server-fetch"] }),
        );
        s.write(&m).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("_config_version: 33"), "其余段保留");
        assert!(text.contains("provider: openai-api"));
        assert!(text.contains("fetch:"));
        assert!(text.contains("mcp-server-fetch"));
        // 读回
        let back = s.read().unwrap();
        assert_eq!(back["fetch"]["command"], "uvx");
    }

    #[test]
    fn empty_write_restores_empty_flow() {
        let root = root("empty");
        let path = root.join("config.yaml");
        std::fs::write(&path, SAMPLE).unwrap();
        let s = HermesStore::new(&root);
        let mut m = BTreeMap::new();
        m.insert("a".to_string(), json!({ "command": "x" }));
        s.write(&m).unwrap();
        s.write(&BTreeMap::new()).unwrap();
        assert_eq!(s.read().unwrap().len(), 0);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("mcp_servers: {}"), "空段回归空映射形态");
        assert!(text.contains("_config_version: 33"));
    }

    #[test]
    fn write_creates_file_when_missing() {
        let root = root("missing");
        let s = HermesStore::new(&root);
        let mut m = BTreeMap::new();
        m.insert("a".to_string(), json!({ "command": "x" }));
        s.write(&m).unwrap();
        assert!(root.join("config.yaml").exists());
        assert_eq!(s.read().unwrap()["a"]["command"], "x");
    }

    #[test]
    fn parse_failure_refuses_to_touch() {
        let root = root("parse");
        let path = root.join("config.yaml");
        std::fs::write(&path, "model: [broken").unwrap();
        let s = HermesStore::new(&root);
        assert_eq!(s.read().unwrap_err().1, "E_PARSE");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "model: [broken");
    }
}
