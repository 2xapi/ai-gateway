//! Hermes Agent 托管(多平台接入方案 §3.5,B 阶段第一个新平台)
//!
//! 载体 `~/.hermes/config.yaml`(YAML),叠加语义(D1):
//! - `custom_providers:` 列表 upsert 固定条目 `2xapi-gateway`(base_url 指向网关 `/hermes` 前缀,
//!   api_key 写占位——网关入站不校验客户端凭证,真 Key 由网关按供应商覆盖注入);
//! - `model.provider` 指针仅当当前为官方默认/未设置/已指向本条目时切换(D1「默认指针受控切换」);
//! - 用户已有条目与其余段(agent/display/voice/_config_version/mcp_servers…)零触碰。
//!
//! 写入手法:段级文本替换(保注释保其他段)+ 顶层重复键治愈(keep-last,对齐 hermes 自身
//! PyYAML last-wins 语义)+ 原子写 + 写前备份。源码实证见 hermes-agent v0.18.2
//! (config.py `_normalize_custom_provider_entry` / models.py 端点适配):custom provider
//! 由 OpenAI SDK 在 base_url 后追加 `/chat/completions`,故条目 base_url 不带尾斜杠。
//!
//! 路径解析对齐 hermes `get_hermes_home()`:`HERMES_HOME` 环境变量(trim 后非空)优先,
//! 默认 `~/.hermes`(Win 平台差异本期不做,见交接日志备案)。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::config::GATEWAY_BASE_URL;
use crate::providers::Provider;

/// 本产品在 hermes `custom_providers:` 里的固定条目名(D1)。
pub const ENTRY_NAME: &str = "2xapi-gateway";
/// 指针无快照可恢复时回退的 hermes 官方默认 provider(v0.18.2 全新安装形态)。
pub const DEFAULT_OFFICIAL_PROVIDER: &str = "openai-api";
/// 备份文件前缀(与 codex 的 `config-apply` 命名空间隔离,快照恢复按此过滤)。
const BACKUP_PREFIX: &str = "hermes-config";

pub type OpError = (u16, String, String);

// ── 路径 ─────────────────────────────────────────────────────

/// hermes 配置根目录(对齐 hermes 自身解析:HERMES_HOME 非空优先,默认 ~/.hermes)。
pub fn hermes_home() -> PathBuf {
    if let Some(raw) = std::env::var_os("HERMES_HOME") {
        let v = raw.to_string_lossy();
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join(".hermes")
}

// ── YAML 段级读写(文本层,保格式) ────────────────────────────

/// 判断一行是否 YAML 顶层键(列 0 起、非注释、非列表项、冒号后为空白/行尾;容忍 \r)。
fn is_top_level_key_line(line: &str) -> bool {
    if line.is_empty() {
        return false;
    }
    let first = line.as_bytes()[0];
    if first == b' ' || first == b'\t' || first == b'#' || first == b'-' {
        return false;
    }
    match line.find(':') {
        Some(pos) => {
            let after = &line[pos + 1..];
            after.is_empty() || after.starts_with([' ', '\t', '\r', '\n'])
        }
        None => false,
    }
}

/// 定位顶层段 `key:` 的字节区间(键行起,至下一顶层键行前);找不到返回 None。
fn find_section_range(raw: &str, key: &str) -> Option<(usize, usize)> {
    let target = format!("{key}:");
    let mut start = None;
    let mut offset = 0;
    for line in raw.split('\n') {
        if start.is_none() && is_top_level_key_line(line) && line.starts_with(&target) {
            let after = &line[target.len()..];
            if after.is_empty() || after.starts_with([' ', '\t', '\r']) {
                start = Some(offset);
            }
        } else if start.is_some() && is_top_level_key_line(line) {
            return Some((start.unwrap(), offset));
        }
        offset += line.len() + 1;
    }
    start.map(|s| (s, raw.len()))
}

/// 顶层重复键治愈:同名段保留最后一次出现(与 hermes 自身 PyYAML last-wins 一致)。
/// 重复源于段替换退化为 append 的历史 bug;读侧治愈后再解析,避免 serde_yaml 拒绝重复键。
fn deduplicate_top_level_keys(raw: &str) -> String {
    // 收集每个顶层键的行索引与字节偏移
    let lines: Vec<&str> = raw.split('\n').collect();
    let mut byte_off = 0usize;
    let mut key_lines: Vec<(usize, usize, &str)> = Vec::new(); // (line_idx, byte_start, key)
    for (i, line) in lines.iter().enumerate() {
        if is_top_level_key_line(line) {
            if let Some(pos) = line.find(':') {
                key_lines.push((i, byte_off, &line[..pos]));
            }
        }
        byte_off += line.len() + 1;
    }
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for &(_, _, key) in &key_lines {
        *counts.entry(key).or_insert(0) += 1;
    }
    if counts.values().all(|c| *c <= 1) {
        return raw.to_string();
    }
    // 重写:首个段之前的内容(注释/文档头)保留;每段取到下一顶层键行(或 EOF),
    // 只保留同名键的最后一次出现
    let mut result = String::with_capacity(raw.len());
    let head_end = key_lines
        .first()
        .map(|&(_, start, _)| start)
        .unwrap_or(raw.len());
    result.push_str(&raw[..head_end]);
    for (idx, &(_, start, key)) in key_lines.iter().enumerate() {
        let remaining = counts.get_mut(key).unwrap();
        *remaining -= 1;
        if *remaining > 0 {
            continue; // 后面还有同名段,丢弃这份旧的
        }
        let end = key_lines
            .get(idx + 1)
            .map(|&(_, s, _)| s)
            .unwrap_or(raw.len());
        result.push_str(&raw[start..end]);
    }
    result
}

/// 从文本中移除某顶层段的**全部**出现(替换后清扫残留重复)。
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

/// 序列化 `key: <value>` 为一段 YAML 文本(值经 serde_yaml,Mapping 保序)。
fn section_to_yaml(key: &str, value: &serde_yaml::Value) -> Result<String, String> {
    let mut wrap = serde_yaml::Mapping::new();
    wrap.insert(serde_yaml::Value::String(key.to_string()), value.clone());
    serde_yaml::to_string(&serde_yaml::Value::Mapping(wrap))
        .map_err(|e| format!("YAML 段序列化失败({key}): {e}"))
}

/// 段级替换:命中则原位替换(并清扫其后同名残留),未命中则追加到文件尾。
fn replace_section(raw: &str, key: &str, value: &serde_yaml::Value) -> Result<String, String> {
    let serialized = section_to_yaml(key, value)?;
    Ok(match find_section_range(raw, key) {
        Some((start, end)) => {
            let mut out = String::with_capacity(raw.len());
            out.push_str(&raw[..start]);
            out.push_str(&serialized);
            let remainder = remove_all_sections(&raw[end..], key);
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

/// 读 config.yaml 为 YAML Value;文件缺失/空白 → 空 Mapping;重复键先治愈;解析失败 → Err。
fn read_hermes_yaml(path: &Path) -> Result<serde_yaml::Value, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(serde_yaml::Value::Mapping(Default::default()))
        }
        Err(e) => return Err(format!("读取 {} 失败: {e}", path.display())),
    };
    if raw.trim().is_empty() {
        return Ok(serde_yaml::Value::Mapping(Default::default()));
    }
    let healed = deduplicate_top_level_keys(&raw);
    serde_yaml::from_str(&healed).map_err(|e| format!("解析 config.yaml 失败: {e}"))
}

/// 原子写文本(同工程 .tmp+rename 惯例)。
fn write_text_atomic(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败: {e}"))?;
    }
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, content).map_err(|e| format!("写临时文件失败: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("落盘失败: {e}"))
}

/// 备份 config.yaml(文件名 hermes-config-<ts>.yaml + manifest,与 codex 备份命名空间隔离)。
fn backup_yaml_file(src: &Path, backup_dir: &Path, purpose: &str) -> Result<(), String> {
    if !src.exists() {
        return Ok(()); // 首次托管前无文件,无需备份(host 会新建)
    }
    std::fs::create_dir_all(backup_dir).map_err(|e| e.to_string())?;
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S%.3f").to_string();
    let backup_path = backup_dir.join(format!("{BACKUP_PREFIX}-{timestamp}.yaml"));
    let data = std::fs::read(src).map_err(|e| e.to_string())?;
    std::fs::write(&backup_path, &data).map_err(|e| e.to_string())?;
    let manifest = json!({
        "version": 1,
        "kind": "hermes-config",
        "purpose": purpose,
        "createdAt": chrono::Local::now().to_rfc3339(),
        "configPath": src.to_string_lossy(),
        "backupFile": backup_path.file_name().and_then(|name| name.to_str()),
        "sha256": {
            "algo": "sha256",
            "note": "see file bytes"
        },
    });
    let manifest_path = PathBuf::from(format!("{}.manifest.json", backup_path.display()));
    let manifest_raw = serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?;
    if let Err(error) = std::fs::write(&manifest_path, manifest_raw) {
        let cleanup = std::fs::remove_file(&backup_path).err();
        return Err(match cleanup {
            Some(cleanup_error) => format!(
                "写备份 manifest 失败: {error}；清理无 manifest 的备份 {} 失败: {cleanup_error}",
                backup_path.display()
            ),
            None => format!("写备份 manifest 失败: {error}"),
        });
    }
    Ok(())
}

/// 找最近的 hermes pre-host 快照(unhost 时恢复 model 段指针)。
fn find_pre_host_snapshot(
    backup_dir: &Path,
    config_path: &Path,
) -> Result<Option<serde_yaml::Value>, OpError> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let rd = match std::fs::read_dir(backup_dir) {
        Ok(rd) => rd,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err((500, "E_IO".into(), error.to_string())),
    };
    let normalized_target = normalize_path(config_path);
    let normalized_backup_dir = normalize_path(backup_dir);
    for entry in rd {
        let e = entry.map_err(|error| (500, "E_IO".into(), error.to_string()))?;
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with(BACKUP_PREFIX) || !name.ends_with(".manifest.json") {
            continue;
        }
        let manifest_raw = std::fs::read_to_string(e.path())
            .map_err(|error| (500, "E_IO".into(), error.to_string()))?;
        let manifest: Value = serde_json::from_str(&manifest_raw).map_err(|error| {
            (
                422,
                "E_BACKUP_MANIFEST".into(),
                format!("Hermes 备份 manifest 损坏: {error}"),
            )
        })?;
        if manifest.get("version").and_then(Value::as_u64) != Some(1)
            || manifest.get("kind").and_then(Value::as_str) != Some("hermes-config")
        {
            return Err((
                422,
                "E_BACKUP_MANIFEST".into(),
                "Hermes 备份 manifest 类型或版本不受支持".into(),
            ));
        }
        if manifest.get("purpose").and_then(|v| v.as_str()) != Some("pre-host") {
            continue;
        }
        let target = manifest.get("configPath").and_then(Value::as_str).ok_or((
            422,
            "E_BACKUP_MANIFEST".into(),
            "Hermes 备份 manifest 缺少 configPath".into(),
        ))?;
        if normalize_path(Path::new(target)) != normalized_target {
            continue;
        }
        let derived_name = name.trim_end_matches(".manifest.json");
        let backup_name = manifest
            .get("backupFile")
            .and_then(Value::as_str)
            .unwrap_or(derived_name);
        if Path::new(backup_name)
            .file_name()
            .and_then(|value| value.to_str())
            != Some(backup_name)
            || backup_name != derived_name
        {
            return Err((
                422,
                "E_BACKUP_MANIFEST".into(),
                "Hermes 备份 manifest 的 backupFile 非法".into(),
            ));
        }
        let yaml_path = backup_dir.join(backup_name);
        if normalize_path(&yaml_path)
            .parent()
            .is_none_or(|parent| parent != normalized_backup_dir)
        {
            return Err((
                422,
                "E_BACKUP_TARGET".into(),
                "Hermes 备份文件不在受控备份目录".into(),
            ));
        }
        let mtime = yaml_path
            .metadata()
            .and_then(|m| m.modified())
            .map_err(|error| {
                (
                    422,
                    "E_BACKUP_TARGET".into(),
                    format!("Hermes 备份文件不可用: {error}"),
                )
            })?;
        if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            best = Some((mtime, yaml_path));
        }
    }
    let Some(path) = best.map(|(_, path)| path) else {
        return Ok(None);
    };
    let raw =
        std::fs::read_to_string(path).map_err(|error| (500, "E_IO".into(), error.to_string()))?;
    let healed = deduplicate_top_level_keys(&raw);
    serde_yaml::from_str(&healed).map(Some).map_err(|error| {
        (
            422,
            "E_BACKUP_CONFIG".into(),
            format!("Hermes pre-host 快照损坏: {error}"),
        )
    })
}

fn normalize_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => parent
            .canonicalize()
            .map(|canonical| canonical.join(name))
            .unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    }
}

// ── 条目构造 ─────────────────────────────────────────────────

/// 由供应商生成 `custom_providers:` 条目(models 为 dict 形状,key=模型 id)。
fn build_gateway_entry(provider: &Provider) -> serde_yaml::Mapping {
    let mut models = serde_yaml::Mapping::new();
    if provider.models.is_empty() {
        models.insert(
            serde_yaml::Value::String(provider.model.clone()),
            serde_yaml::Value::Mapping(Default::default()),
        );
    } else {
        for m in &provider.models {
            let mut meta = serde_yaml::Mapping::new();
            if let Some(cw) = m.context_window {
                meta.insert(
                    serde_yaml::Value::String("context_length".into()),
                    serde_yaml::Value::Number((cw as i64).into()),
                );
            }
            models.insert(
                serde_yaml::Value::String(m.name.clone()),
                serde_yaml::Value::Mapping(meta),
            );
        }
    }
    let mut entry = serde_yaml::Mapping::new();
    entry.insert("name".into(), ENTRY_NAME.into());
    // base_url 指向网关 /hermes 前缀:OpenAI SDK 自动追加 /chat/completions → 命中专属入口
    entry.insert(
        "base_url".into(),
        format!("{GATEWAY_BASE_URL}/hermes").into(),
    );
    // 占位 Key:网关入站不校验客户端凭证(一律按供应商覆盖注入);不写真 Key,盘上零泄漏
    entry.insert("api_key".into(), ENTRY_NAME.into());
    entry.insert("api_mode".into(), "chat_completions".into());
    entry.insert("model".into(), provider.model.clone().into());
    entry.insert("models".into(), serde_yaml::Value::Mapping(models));
    entry
}

/// 当前指针指向的用户第三方条目名集合(custom_providers 里非本产品的条目)。
fn user_entry_names(doc: &serde_yaml::Value) -> Vec<String> {
    doc.get("custom_providers")
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|e| e.get("name").and_then(|n| n.as_str()))
                .filter(|n| *n != ENTRY_NAME)
                .map(|n| n.to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// 本产品条目是否在盘(custom_providers 里存在 name==2xapi-gateway)。
fn entry_exists(doc: &serde_yaml::Value) -> bool {
    doc.get("custom_providers")
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .any(|e| e.get("name").and_then(|n| n.as_str()) == Some(ENTRY_NAME))
        })
        .unwrap_or(false)
}

/// 当前 model.provider 指针值(无段/无字段 → None)。
fn current_pointer(doc: &serde_yaml::Value) -> Option<String> {
    doc.get("model")?
        .get("provider")?
        .as_str()
        .map(|s| s.to_string())
}

// ── state / host / unhost ────────────────────────────────────

/// hermes 托管态(受控标记=条目存在性,禁地址匹配红线;与 codex hosting 信封同构)。
pub fn detect_state(config_path: &Path) -> Value {
    let doc =
        read_hermes_yaml(config_path).unwrap_or(serde_yaml::Value::Mapping(Default::default()));
    let hosted = entry_exists(&doc);
    json!({
        "hosting": if hosted { json!({ "way": "gateway", "entry": ENTRY_NAME }) } else { Value::Null },
        "pointer": current_pointer(&doc),
        "configPath": config_path.to_string_lossy(),
    })
}

/// 清除 Hermes 自己的 active，不影响其他平台。
fn clear_active_if_hermes(providers_path: &Path) -> Result<(), OpError> {
    crate::desktop::clear_active_checked(providers_path, "hermes")
}

/// host:custom_providers upsert 2xapi-gateway + 指针受控切换;way 仅 gateway(叠加平台无 direct)。
pub fn host(
    config_path: &Path,
    backup_dir: &Path,
    providers_path: &Path,
    provider_id: &str,
    way: &str,
) -> Result<Value, OpError> {
    if way != "gateway" {
        return Err((
            400,
            "E_BAD_WAY".into(),
            "Hermes 为叠加平台,仅支持 gateway 托管方式".into(),
        ));
    }
    let data = crate::providers::load(providers_path);
    let provider = data
        .providers
        .iter()
        .find(|p| p.id == provider_id)
        .cloned()
        .ok_or_else(|| {
            (
                404,
                "E_PROVIDER_NOT_FOUND".to_string(),
                "找不到该 hermes 供应商".to_string(),
            )
        })?;
    crate::desktop::validate_provider_agent(&provider, "hermes")?;
    if provider.model.is_empty() {
        return Err((
            422,
            "E_NO_MODEL".to_string(),
            "该供应商未配置默认模型,请先在编辑里拉取模型或手填".to_string(),
        ));
    }

    let raw = match std::fs::read_to_string(config_path) {
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
    let healed = deduplicate_top_level_keys(&raw);
    let doc: serde_yaml::Value = serde_yaml::from_str(&healed).map_err(|e| {
        (
            500,
            "E_CONFIG".to_string(),
            format!("解析 config.yaml 失败: {e}"),
        )
    })?;

    // 指针受控切换(D1):当前指向用户第三方条目 → 不动指针;否则(官方/未设置/已指向本条目)→ 切
    let pointer_now = current_pointer(&doc);
    let user_names = user_entry_names(&doc);
    let pointer_switched =
        !matches!(&pointer_now, Some(p) if p != ENTRY_NAME && user_names.contains(p));

    // upsert custom_providers(用户条目原样保留)
    let mut providers_seq: Vec<serde_yaml::Value> = doc
        .get("custom_providers")
        .and_then(|v| v.as_sequence())
        .cloned()
        .unwrap_or_default();
    let entry = serde_yaml::Value::Mapping(build_gateway_entry(&provider));
    match providers_seq
        .iter()
        .position(|e| e.get("name").and_then(|n| n.as_str()) == Some(ENTRY_NAME))
    {
        Some(idx) => providers_seq[idx] = entry,
        None => providers_seq.push(entry),
    }

    // model 段:指针切换时更新 provider/default;否则仅保留原段(条目已写入,UI 提示用户可自选)
    let mut new_text = replace_section(
        &healed,
        "custom_providers",
        &serde_yaml::Value::Sequence(providers_seq),
    )
    .map_err(|e| (500, "E_CONFIG".to_string(), e))?;
    let mut switched = false;
    if pointer_switched {
        let mut model = match doc.get("model") {
            Some(serde_yaml::Value::Mapping(m)) => m.clone(),
            _ => serde_yaml::Mapping::new(),
        };
        model.insert("provider".into(), ENTRY_NAME.into());
        model.insert("default".into(), provider.model.clone().into());
        new_text = replace_section(&new_text, "model", &serde_yaml::Value::Mapping(model))
            .map_err(|e| (500, "E_CONFIG".to_string(), e))?;
        switched = true;
    }

    // 幂等:序列化结果与治愈后原文相同 → no-op(但仍 set_active,对齐 codex 语义)
    let paths = [config_path.to_path_buf(), providers_path.to_path_buf()];
    let snapshots = paths
        .iter()
        .map(|path| crate::desktop::snapshot_file(path).map(|snapshot| (path.clone(), snapshot)))
        .collect::<Result<Vec<_>, _>>()?;
    let outcome = (|| {
        let config_written = if new_text != healed {
            let already = entry_exists(&doc);
            let purpose = if already { "pre-switch" } else { "pre-host" };
            backup_yaml_file(config_path, backup_dir, purpose)
                .map_err(|e| (500, "E_IO".to_string(), e))?;
            write_text_atomic(config_path, &new_text).map_err(|e| (500, "E_IO".to_string(), e))?;
            true
        } else {
            false
        };

        crate::desktop::set_active_checked(providers_path, &provider, "hermes")?;

        Ok(json!({
            "hosted": true,
            "switched": switched,
            "pointerSwitched": switched,
            "entryWritten": true,
            "hosting": detect_state(config_path)["hosting"].clone(),
            "changed": { "config": config_written },
        }))
    })();
    outcome.map_err(|error| crate::desktop::rollback_files(error, &snapshots))
}

/// unhost:仅移除本产品条目;指针指向本条目时恢复(快照优先,无快照回官方默认)。
pub fn unhost(
    config_path: &Path,
    backup_dir: &Path,
    providers_path: &Path,
) -> Result<Value, OpError> {
    let raw = match std::fs::read_to_string(config_path) {
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
    let healed = deduplicate_top_level_keys(&raw);
    let doc: serde_yaml::Value = serde_yaml::from_str(&healed).map_err(|e| {
        (
            500,
            "E_CONFIG".to_string(),
            format!("解析 config.yaml 失败: {e}"),
        )
    })?;

    if !entry_exists(&doc) {
        // 未托管(或用户自己删过)→ 幂等 no-op
        clear_active_if_hermes(providers_path)?;
        return Ok(json!({ "restored": false, "alreadyClean": true }));
    }

    // 移除本条目;段空则整段移除
    let mut providers_seq: Vec<serde_yaml::Value> = doc
        .get("custom_providers")
        .and_then(|v| v.as_sequence())
        .cloned()
        .unwrap_or_default();
    providers_seq.retain(|e| e.get("name").and_then(|n| n.as_str()) != Some(ENTRY_NAME));
    let mut new_text = if providers_seq.is_empty() {
        remove_all_sections(&healed, "custom_providers")
    } else {
        replace_section(
            &healed,
            "custom_providers",
            &serde_yaml::Value::Sequence(providers_seq),
        )
        .map_err(|e| (500, "E_CONFIG".to_string(), e))?
    };

    // 指针恢复:仅当当前指向本条目
    let pointer_now = current_pointer(&doc);
    let mut pointer_restored = false;
    if pointer_now.as_deref() == Some(ENTRY_NAME) {
        let restored_model = find_pre_host_snapshot(backup_dir, config_path)?
            .and_then(|snap| snap.get("model").cloned());
        match restored_model {
            Some(m) => {
                new_text = replace_section(&new_text, "model", &m)
                    .map_err(|e| (500, "E_CONFIG".to_string(), e))?;
            }
            None => {
                // 无快照:回官方默认形态(指针曾指向我们 = host 时我们切过)
                let mut model = match doc.get("model") {
                    Some(serde_yaml::Value::Mapping(m)) => m.clone(),
                    _ => serde_yaml::Mapping::new(),
                };
                model.insert("provider".into(), DEFAULT_OFFICIAL_PROVIDER.into());
                new_text = replace_section(&new_text, "model", &serde_yaml::Value::Mapping(model))
                    .map_err(|e| (500, "E_CONFIG".to_string(), e))?;
            }
        }
        pointer_restored = true;
    }

    let paths = [config_path.to_path_buf(), providers_path.to_path_buf()];
    let snapshots = paths
        .iter()
        .map(|path| crate::desktop::snapshot_file(path).map(|snapshot| (path.clone(), snapshot)))
        .collect::<Result<Vec<_>, _>>()?;
    let outcome = (|| {
        backup_yaml_file(config_path, backup_dir, "pre-unhost")
            .map_err(|e| (500, "E_IO".to_string(), e))?;
        write_text_atomic(config_path, &new_text).map_err(|e| (500, "E_IO".to_string(), e))?;
        clear_active_if_hermes(providers_path)?;

        Ok(json!({
            "restored": true,
            "pointerRestored": pointer_restored,
            "hosting": detect_state(config_path)["hosting"].clone(),
        }))
    })();
    outcome.map_err(|error| crate::desktop::rollback_files(error, &snapshots))
}

/// POST /api/desktop/hermes/start —— 未托管 409;托管后返回直接运行提示
/// (hermes 为整平台托管,条目含真实 base/key,命令本体无需 env 前缀,providerId 仅回显)。
pub fn start(
    config_path: &Path,
    providers_path: &Path,
    provider_id: &str,
) -> Result<Value, OpError> {
    if detect_state(config_path)["hosting"].is_null() {
        return Err((409, "E_NOT_HOSTED".into(), "请先托管,再启动".into()));
    }
    super::cli_start_response(providers_path, provider_id, "hermes chat")
}

// ── tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{AccessMode, ModelConfig, ProviderData, WireApi};

    fn tmpdir(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("hermes-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.yaml");
        let backup = dir.join("backups");
        std::fs::create_dir_all(&backup).unwrap();
        (dir, config, backup)
    }

    fn provider_fixture(id: &str, agent: &str, model: &str) -> Provider {
        Provider {
            id: id.into(),
            name: "Test".into(),
            agent: agent.into(),
            icon: None,
            icon_color: None,
            sort_index: 0,
            created_at: 0,
            website_url: None,
            notes: None,
            base_url: "https://2xa.cc.cd".into(),
            api_key: "sk-test".into(),
            keys: vec![],
            access_mode: AccessMode::default(),
            wire_api: WireApi::default(),
            user_agent: None,
            model: model.into(),
            models: vec![],
            claude_desktop_model_routes: vec![],
            context_window: None,
            proxy_url: None,
            timeout_secs: None,
            sub2api_enabled: false,
            sub2api_multiplier: 1.0,
            custom_headers: None,
            config_toml_snapshot: None,
            auth_json_snapshot: None,
            reasoning_levels: None,
        }
    }

    fn providers_file(dir: &Path, providers: Vec<Provider>) -> PathBuf {
        let path = dir.join("providers.json");
        let data = ProviderData {
            schema_version: 1,
            active_provider_id: None,
            active_provider_ids: Default::default(),
            providers,
        };
        std::fs::write(&path, serde_json::to_string(&data).unwrap()).unwrap();
        path
    }

    const USER_CONFIG: &str = "model:\n  provider: openai-api\n  default: gpt-5.5\nagent:\n  service_tier: normal\n  reasoning_effort: max\ndisplay:\n  language: zh\nvoice:\n  auto_tts: false\n_config_version: 33\nmcp_servers: {}\n";

    /// host 后:条目写入、指针切换、用户段零触碰。
    #[test]
    fn host_writes_entry_and_switches_pointer() {
        let (dir, config, backup) = tmpdir("host-basic");
        std::fs::write(&config, USER_CONFIG).unwrap();
        let providers = providers_file(&dir, vec![provider_fixture("p1", "hermes", "gpt-5.5")]);
        let out = host(&config, &backup, &providers, "p1", "gateway").unwrap();
        assert_eq!(out["hosted"], json!(true));
        assert_eq!(out["switched"], json!(true));
        let doc = read_hermes_yaml(&config).unwrap();
        // 条目形状
        let entries = doc.get("custom_providers").unwrap().as_sequence().unwrap();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.get("name").unwrap().as_str(), Some(ENTRY_NAME));
        assert_eq!(
            e.get("base_url").unwrap().as_str(),
            Some("http://127.0.0.1:8787/hermes")
        );
        assert_eq!(
            e.get("api_mode").unwrap().as_str(),
            Some("chat_completions")
        );
        assert_eq!(e.get("model").unwrap().as_str(), Some("gpt-5.5"));
        assert!(
            e.get("api_key").unwrap().as_str() != Some("sk-test"),
            "真 Key 不得落盘"
        );
        // 指针
        assert_eq!(current_pointer(&doc).as_deref(), Some(ENTRY_NAME));
        // 用户段零触碰
        assert_eq!(
            doc.get("agent")
                .unwrap()
                .get("reasoning_effort")
                .unwrap()
                .as_str(),
            Some("max")
        );
        assert_eq!(
            doc.get("display")
                .unwrap()
                .get("language")
                .unwrap()
                .as_str(),
            Some("zh")
        );
        assert_eq!(doc.get("_config_version").unwrap().as_i64(), Some(33));
        assert!(doc.get("voice").is_some());
    }

    /// 幂等:同供应商再 host → no-op(不新增备份、不重复条目)。
    #[test]
    fn host_is_idempotent() {
        let (dir, config, backup) = tmpdir("host-idem");
        std::fs::write(&config, USER_CONFIG).unwrap();
        let providers = providers_file(&dir, vec![provider_fixture("p1", "hermes", "gpt-5.5")]);
        host(&config, &backup, &providers, "p1", "gateway").unwrap();
        let first = std::fs::read_to_string(&config).unwrap();
        let backup_count = std::fs::read_dir(&backup).unwrap().count();
        let out = host(&config, &backup, &providers, "p1", "gateway").unwrap();
        assert_eq!(
            out["changed"]["config"],
            json!(false),
            "第二次 host 应 no-op"
        );
        assert_eq!(std::fs::read_to_string(&config).unwrap(), first);
        assert_eq!(std::fs::read_dir(&backup).unwrap().count(), backup_count);
    }

    /// 用户第三方指针:指针不动,条目照写(UI 提示用户可在 hermes 内自选)。
    #[test]
    fn host_keeps_user_third_party_pointer() {
        let (dir, config, backup) = tmpdir("host-userptr");
        std::fs::write(
            &config,
            "model:\n  provider: my-router\n  default: qwen3\ncustom_providers:\n  - name: my-router\n    base_url: https://openrouter.ai/api/v1\n    api_key: sk-or-user\n",
        )
        .unwrap();
        let providers = providers_file(&dir, vec![provider_fixture("p1", "hermes", "gpt-5.5")]);
        let out = host(&config, &backup, &providers, "p1", "gateway").unwrap();
        assert_eq!(out["switched"], json!(false), "用户指针不得擅动");
        let doc = read_hermes_yaml(&config).unwrap();
        assert_eq!(current_pointer(&doc).as_deref(), Some("my-router"));
        let entries = doc.get("custom_providers").unwrap().as_sequence().unwrap();
        assert_eq!(entries.len(), 2, "用户条目保留 + 本产品条目");
        let user_entry = entries
            .iter()
            .find(|e| e.get("name").and_then(|n| n.as_str()) == Some("my-router"))
            .unwrap();
        assert_eq!(
            user_entry.get("api_key").unwrap().as_str(),
            Some("sk-or-user"),
            "用户条目零触碰"
        );
    }

    /// 换供应商:条目更新,指针若已指向本条目保持指向,pre-switch 备份。
    #[test]
    fn host_switches_provider_updates_entry() {
        let (dir, config, backup) = tmpdir("host-switch");
        std::fs::write(&config, USER_CONFIG).unwrap();
        let providers = providers_file(
            &dir,
            vec![
                provider_fixture("p1", "hermes", "gpt-5.5"),
                provider_fixture("p2", "hermes", "glm-5"),
            ],
        );
        host(&config, &backup, &providers, "p1", "gateway").unwrap();
        let out = host(&config, &backup, &providers, "p2", "gateway").unwrap();
        assert_eq!(
            out["switched"],
            json!(true),
            "指针已指向本条目,换供应商后 default 更新"
        );
        let doc = read_hermes_yaml(&config).unwrap();
        let entries = doc.get("custom_providers").unwrap().as_sequence().unwrap();
        assert_eq!(entries.len(), 1, "upsert 不新增");
        assert_eq!(entries[0].get("model").unwrap().as_str(), Some("glm-5"));
        assert_eq!(
            doc.get("model").unwrap().get("default").unwrap().as_str(),
            Some("glm-5")
        );
    }

    /// 串台防护:codex 供应商不能 host 给 hermes。
    #[test]
    fn host_rejects_foreign_agent_provider() {
        let (dir, config, backup) = tmpdir("host-foreign");
        std::fs::write(&config, USER_CONFIG).unwrap();
        let providers = providers_file(&dir, vec![provider_fixture("p1", "codex", "gpt-5.5")]);
        let err = host(&config, &backup, &providers, "p1", "gateway").unwrap_err();
        assert_eq!(err.1, "E_PROVIDER_AGENT_MISMATCH");
        assert!(!config.exists() || std::fs::read_to_string(&config).unwrap() == USER_CONFIG);
    }

    /// 无模型拒绝(E_NO_MODEL,对齐 codex)。
    #[test]
    fn host_rejects_provider_without_model() {
        let (dir, config, backup) = tmpdir("host-nomodel");
        std::fs::write(&config, USER_CONFIG).unwrap();
        let providers = providers_file(&dir, vec![provider_fixture("p1", "hermes", "")]);
        let err = host(&config, &backup, &providers, "p1", "gateway").unwrap_err();
        assert_eq!(err.1, "E_NO_MODEL");
    }

    /// way 仅 gateway。
    #[test]
    fn host_rejects_direct_way() {
        let (dir, config, backup) = tmpdir("host-way");
        let providers = providers_file(&dir, vec![provider_fixture("p1", "hermes", "m")]);
        let err = host(&config, &backup, &providers, "p1", "direct").unwrap_err();
        assert_eq!(err.1, "E_BAD_WAY");
    }

    /// unhost 还原 = host 前快照(含用户第三方条目与指针)。
    #[test]
    fn unhost_restores_pre_host_state() {
        let (dir, config, backup) = tmpdir("unhost-restore");
        let original = "model:\n  provider: my-router\n  default: qwen3\nagent:\n  reasoning_effort: max\ncustom_providers:\n  - name: my-router\n    base_url: https://openrouter.ai/api/v1\n    api_key: sk-or-user\n_config_version: 33\n".to_string();
        std::fs::write(&config, &original).unwrap();
        let providers = providers_file(&dir, vec![provider_fixture("p1", "hermes", "gpt-5.5")]);
        // 指针指向用户第三方 → host 不切指针;unhost 后应与原文件语义一致
        host(&config, &backup, &providers, "p1", "gateway").unwrap();
        let out = unhost(&config, &backup, &providers).unwrap();
        assert_eq!(out["restored"], json!(true));
        let doc = read_hermes_yaml(&config).unwrap();
        assert!(!entry_exists(&doc));
        assert_eq!(current_pointer(&doc).as_deref(), Some("my-router"));
        assert_eq!(doc.get("_config_version").unwrap().as_i64(), Some(33));
    }

    #[test]
    fn unhost_clears_only_hermes_active() {
        let (dir, config, backup) = tmpdir("unhost-active-scope");
        std::fs::write(&config, USER_CONFIG).unwrap();
        let providers = providers_file(
            &dir,
            vec![
                provider_fixture("codex-p", "codex", "gpt-5.5"),
                provider_fixture("hermes-p", "hermes", "gpt-5.5"),
            ],
        );
        crate::providers::set_active(&providers, "codex-p");
        host(&config, &backup, &providers, "hermes-p", "gateway").unwrap();
        unhost(&config, &backup, &providers).unwrap();

        let data = crate::providers::load(&providers);
        assert_eq!(
            data.active_provider_ids.get("codex").map(String::as_str),
            Some("codex-p")
        );
        assert!(!data.active_provider_ids.contains_key("hermes"));
    }

    /// unhost 指针恢复:快照优先(官方指针场景)。
    #[test]
    fn unhost_restores_pointer_from_snapshot() {
        let (dir, config, backup) = tmpdir("unhost-ptr");
        std::fs::write(&config, USER_CONFIG).unwrap();
        let providers = providers_file(&dir, vec![provider_fixture("p1", "hermes", "gpt-5.5")]);
        host(&config, &backup, &providers, "p1", "gateway").unwrap();
        let out = unhost(&config, &backup, &providers).unwrap();
        assert_eq!(out["pointerRestored"], json!(true));
        let doc = read_hermes_yaml(&config).unwrap();
        assert!(!entry_exists(&doc));
        assert_eq!(
            current_pointer(&doc).as_deref(),
            Some("openai-api"),
            "恢复 host 前官方指针"
        );
        assert_eq!(
            doc.get("model").unwrap().get("default").unwrap().as_str(),
            Some("gpt-5.5")
        );
    }

    #[test]
    fn unhost_ignores_pre_host_manifest_for_another_config_path() {
        let (dir, config, backup) = tmpdir("unhost-target-match");
        std::fs::write(&config, USER_CONFIG).unwrap();
        let providers = providers_file(&dir, vec![provider_fixture("p1", "hermes", "gpt-5.5")]);
        host(&config, &backup, &providers, "p1", "gateway").unwrap();

        let foreign_config = dir.join("other-config.yaml");
        let foreign_name = "hermes-config-foreign.yaml";
        std::fs::write(
            backup.join(foreign_name),
            "model:\n  provider: attacker-router\n  default: attacker-model\n",
        )
        .unwrap();
        std::fs::write(
            backup.join(format!("{foreign_name}.manifest.json")),
            json!({
                "version": 1,
                "kind": "hermes-config",
                "purpose": "pre-host",
                "configPath": foreign_config,
                "backupFile": foreign_name,
            })
            .to_string(),
        )
        .unwrap();

        unhost(&config, &backup, &providers).unwrap();

        let restored = read_hermes_yaml(&config).unwrap();
        assert_eq!(current_pointer(&restored).as_deref(), Some("openai-api"));
        assert_eq!(
            restored
                .get("model")
                .unwrap()
                .get("default")
                .unwrap()
                .as_str(),
            Some("gpt-5.5")
        );
    }

    /// 未托管时 unhost 幂等 no-op。
    #[test]
    fn unhost_noop_when_not_hosted() {
        let (dir, config, backup) = tmpdir("unhost-noop");
        std::fs::write(&config, USER_CONFIG).unwrap();
        let providers = providers_file(&dir, vec![]);
        let out = unhost(&config, &backup, &providers).unwrap();
        assert_eq!(out["alreadyClean"], json!(true));
        assert_eq!(std::fs::read_to_string(&config).unwrap(), USER_CONFIG);
    }

    /// host 前 config.yaml 不存在(全新环境):新建文件,仅含本产品段。
    #[test]
    fn host_creates_config_when_missing() {
        let (dir, config, backup) = tmpdir("host-new");
        let providers = providers_file(&dir, vec![provider_fixture("p1", "hermes", "gpt-5.5")]);
        let out = host(&config, &backup, &providers, "p1", "gateway").unwrap();
        assert_eq!(out["hosted"], json!(true));
        let doc = read_hermes_yaml(&config).unwrap();
        assert!(entry_exists(&doc));
        assert_eq!(current_pointer(&doc).as_deref(), Some(ENTRY_NAME));
    }

    /// 重复顶层键治愈:段替换不产生新重复,旧的重复读侧保 last-wins。
    #[test]
    fn deduplicate_top_level_keys_keeps_last() {
        let raw = "model:\n  default: old\nagent:\n  max_turns: 10\nmodel:\n  default: new\n";
        let healed = deduplicate_top_level_keys(raw);
        assert_eq!(healed.matches("model:").count(), 1);
        assert!(healed.contains("default: new"));
        assert!(!healed.contains("default: old"));
        assert!(healed.contains("max_turns"));
    }

    /// CRLF 容忍:Windows 风格行尾不破坏段定位与幂等。
    #[test]
    fn crlf_config_roundtrip_is_idempotent() {
        let (dir, config, backup) = tmpdir("crlf");
        std::fs::write(&config, USER_CONFIG.replace('\n', "\r\n")).unwrap();
        let providers = providers_file(&dir, vec![provider_fixture("p1", "hermes", "gpt-5.5")]);
        host(&config, &backup, &providers, "p1", "gateway").unwrap();
        let first = std::fs::read_to_string(&config).unwrap();
        let out = host(&config, &backup, &providers, "p1", "gateway").unwrap();
        assert_eq!(
            out["changed"]["config"],
            json!(false),
            "CRLF 下幂等不得失效"
        );
        let second = std::fs::read_to_string(&config).unwrap();
        assert_eq!(first, second);
        let doc = read_hermes_yaml(&config).unwrap();
        let entries = doc.get("custom_providers").unwrap().as_sequence().unwrap();
        assert_eq!(entries.len(), 1, "CRLF 下段替换不得退化为 append");
    }

    /// 段定位边界:不误匹配同名前缀键。
    #[test]
    fn find_section_ignores_prefix_keys() {
        let raw = "model_extra:\n  a: 1\nmodel:\n  default: x\n";
        let (start, _) = find_section_range(raw, "model").unwrap();
        assert_eq!(&raw[start..start + 6], "model:");
    }

    /// models dict 形状与 context_length 数值化。
    #[test]
    fn entry_models_dict_shape() {
        let mut p = provider_fixture("p1", "hermes", "glm-5");
        p.models = vec![ModelConfig {
            name: "glm-5".into(),
            display_name: None,
            context_window: Some(200000),
            is_multimodal: false,
            send_as_is: false,
        }];
        let entry = build_gateway_entry(&p);
        let models = entry.get("models").unwrap().as_mapping().unwrap();
        let m = models
            .get(serde_yaml::Value::String("glm-5".into()))
            .unwrap();
        assert_eq!(m.get("context_length").unwrap().as_i64(), Some(200000));
    }

    /// HERMES_HOME 解析:非空优先,空白忽略回默认。
    #[test]
    fn hermes_home_env_precedence() {
        let old = std::env::var_os("HERMES_HOME");
        unsafe { std::env::set_var("HERMES_HOME", "/tmp/hermes-alt-home") };
        assert_eq!(hermes_home(), PathBuf::from("/tmp/hermes-alt-home"));
        unsafe { std::env::set_var("HERMES_HOME", "   ") };
        assert!(hermes_home().ends_with(".hermes"));
        match old {
            Some(v) => unsafe { std::env::set_var("HERMES_HOME", v) },
            None => unsafe { std::env::remove_var("HERMES_HOME") },
        }
    }

    /// detect_state:未托管 hosting=null;host 后 way=gateway。
    #[test]
    fn detect_state_hosting_shape() {
        let (dir, config, backup) = tmpdir("detect");
        std::fs::write(&config, USER_CONFIG).unwrap();
        let providers = providers_file(&dir, vec![provider_fixture("p1", "hermes", "gpt-5.5")]);
        let s0 = detect_state(&config);
        assert!(s0["hosting"].is_null());
        assert_eq!(s0["pointer"].as_str(), Some("openai-api"));
        host(&config, &backup, &providers, "p1", "gateway").unwrap();
        let s1 = detect_state(&config);
        assert_eq!(s1["hosting"]["way"].as_str(), Some("gateway"));
        assert_eq!(s1["pointer"].as_str(), Some(ENTRY_NAME));
    }

    /// providers.rs 不含 hermes 供应商时 host 404。
    #[test]
    fn host_provider_not_found() {
        let (dir, config, backup) = tmpdir("host-404");
        std::fs::write(&config, USER_CONFIG).unwrap();
        let providers = providers_file(&dir, vec![provider_fixture("other", "hermes", "m")]);
        let err = host(&config, &backup, &providers, "p1", "gateway").unwrap_err();
        assert_eq!(err.1, "E_PROVIDER_NOT_FOUND");
    }

    // tmpdir 由 OS 清理 temp 目录,不设显式 cleanup 测试——并行测试下主动删除共享前缀目录
    // 会竞态破坏正在运行的其他用例(实测教训)
}
