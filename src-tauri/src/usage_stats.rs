//! 用量仪表盘后端(竞品对标吸收项;2026-08-17 后端开发部,用户拍板)。
//!
//! - 落盘:网关每请求 append 一行 JSONL 到 `{codex_home}/usage-stats.jsonl`
//!   (ts/provider/key 脱敏/route/line/延迟/ok),不落明文 Key(安全约定与 usage 块一致)。
//! - 聚合:GET /api/usage-stats 有界读取当前文件与单份轮转文件,按 provider 聚合
//!   {count, p50, p90, ok_rate, last_ts}——P50/P90 = 性能自然基准(调研部建议路线)。
//! - 规模:单文件 5 MiB,保留一份轮转,查询最多读取 10 MiB。

use serde::Serialize;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

static USAGE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

fn usage_lock() -> &'static Mutex<()> {
    USAGE_LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(Debug, Clone, Serialize)]
pub struct ReqLog {
    pub ts: i64,
    pub provider_id: String,
    pub provider_name: String,
    pub key_masked: String,
    /// codex | anthropic | gemini | images。
    pub route: String,
    /// 线路 id;直连 = "direct"。
    pub line: String,
    /// 命中加速线路后因线路故障或 per-Key 限额而改走直连。
    pub degraded_to_direct: bool,
    pub latency_ms: u64,
    pub ok: bool,
}

fn log_path(codex_home: &Path) -> std::path::PathBuf {
    codex_home.join("usage-stats.jsonl")
}

fn rotated_log_path(path: &Path) -> std::path::PathBuf {
    path.with_extension("1.jsonl")
}

fn rotate_if_needed(path: &Path, incoming_bytes: u64, max_bytes: u64) -> std::io::Result<()> {
    let current_bytes = match std::fs::metadata(path) {
        Ok(meta) => meta.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error),
    };
    if current_bytes.saturating_add(incoming_bytes) <= max_bytes {
        return Ok(());
    }
    let rotated = rotated_log_path(path);
    match std::fs::remove_file(&rotated) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if current_bytes > 0 {
        std::fs::rename(path, rotated)?;
    }
    Ok(())
}

fn read_recent(path: &Path, max_bytes: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};

    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let Ok(len) = file.metadata().map(|meta| meta.len()) else {
        return String::new();
    };
    let start = len.saturating_sub(max_bytes);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::new();
    if file.take(max_bytes).read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    if start > 0 {
        let Some(line_end) = bytes.iter().position(|byte| *byte == b'\n') else {
            return String::new();
        };
        bytes.drain(..=line_end);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn percentile_nearest_rank(sorted: &[u64], quantile: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((sorted.len() as f64 * quantile).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}

/// Key 脱敏:前 3 + … + 尾 4;过短只留省略号(与 server usage 块同形态,不落明文)。
pub fn mask_key(key: &str) -> String {
    let n = key.chars().count();
    if n >= 8 {
        let head: String = key.chars().take(3).collect();
        let tail: String = key.chars().skip(n - 4).collect();
        format!("{head}…{tail}")
    } else {
        "…".to_string()
    }
}

/// 追加一行请求日志(尽力而为,失败不阻塞网关)。
pub fn log_request(codex_home: &Path, r: &ReqLog) {
    use std::io::Write;
    let Ok(_guard) = usage_lock().lock() else {
        return;
    };
    let Ok(raw) = serde_json::to_string(r) else {
        return;
    };
    if raw.len() as u64 + 1 > MAX_LOG_BYTES {
        return;
    }
    let path = log_path(codex_home);
    if std::fs::create_dir_all(codex_home).is_err()
        || rotate_if_needed(&path, raw.len() as u64 + 1, MAX_LOG_BYTES).is_err()
    {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{raw}");
    }
}

/// 有界读取当前文件与单份轮转文件并聚合(按 provider_id 分组);无数据 → 空。
pub fn summary(codex_home: &Path) -> serde_json::Value {
    use serde_json::Value;
    let Ok(_guard) = usage_lock().lock() else {
        return serde_json::json!({ "providers": [] });
    };
    let path = log_path(codex_home);
    let raw = [rotated_log_path(&path), path]
        .into_iter()
        .map(|path| read_recent(&path, MAX_LOG_BYTES))
        .collect::<Vec<_>>()
        .join("\n");
    let mut by_provider: std::collections::BTreeMap<String, Vec<Value>> = Default::default();
    for line in raw.lines() {
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            if let Some(pid) = v.get("provider_id").and_then(|x| x.as_str()) {
                by_provider.entry(pid.to_string()).or_default().push(v);
            }
        }
    }
    let providers: Vec<Value> = by_provider
        .into_iter()
        .map(|(pid, rows)| {
            let name = rows
                .iter()
                .find_map(|r| r.get("provider_name").and_then(|x| x.as_str()))
                .unwrap_or(&pid)
                .to_string();
            let mut lats: Vec<u64> = rows
                .iter()
                .filter_map(|r| r.get("latency_ms").and_then(|x| x.as_u64()))
                .collect();
            lats.sort_unstable();
            let ok = rows
                .iter()
                .filter(|r| r.get("ok").and_then(|x| x.as_bool()).unwrap_or(false))
                .count();
            let direct_fallbacks = rows
                .iter()
                .filter(|r| {
                    r.get("degraded_to_direct")
                        .and_then(|x| x.as_bool())
                        .unwrap_or(false)
                })
                .count();
            let routes: Vec<&str> = rows
                .iter()
                .filter_map(|r| r.get("route").and_then(|x| x.as_str()))
                .collect();
            let mut routes = routes;
            routes.sort_unstable();
            routes.dedup();
            let last_ts = rows
                .iter()
                .filter_map(|r| r.get("ts").and_then(|x| x.as_i64()))
                .max()
                .unwrap_or(0);
            serde_json::json!({
                "providerId": pid,
                "providerName": name,
                "count": rows.len(),
                "p50Ms": percentile_nearest_rank(&lats, 0.50),
                "p90Ms": percentile_nearest_rank(&lats, 0.90),
                "okRate": if rows.is_empty() { 0.0 } else { ok as f64 / rows.len() as f64 },
                "directFallbackCount": direct_fallbacks,
                "lastTs": last_ts,
                "routes": routes,
            })
        })
        .collect();
    serde_json::json!({ "providers": providers })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("2xapi-us-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn mask_hides_middle() {
        assert_eq!(mask_key("sk-abcdefghijkl"), "sk-…ijkl");
        assert_eq!(mask_key("short"), "…");
    }

    #[test]
    fn log_and_summarize_per_provider_p50_p90() {
        let home = tmp("sum");
        for (i, ok, ms) in [(1, true, 10), (2, true, 20), (3, true, 30), (4, false, 40)] {
            log_request(
                &home,
                &ReqLog {
                    ts: i,
                    provider_id: "p1".into(),
                    provider_name: "P1".into(),
                    key_masked: mask_key("sk-abcdefghijkl"),
                    route: "codex".into(),
                    line: "direct".into(),
                    degraded_to_direct: false,
                    latency_ms: ms,
                    ok,
                },
            );
        }
        let v = summary(&home);
        let prov = &v["providers"][0];
        assert_eq!(prov["providerId"], "p1");
        assert_eq!(prov["count"], 4);
        // 最近秩:4 条 P50=第 2 条 20,P90=第 4 条 40。
        assert_eq!(prov["p50Ms"], 20);
        assert_eq!(prov["p90Ms"], 40);
        assert!((prov["okRate"].as_f64().unwrap() - 0.75).abs() < 1e-9);
        assert_eq!(prov["directFallbackCount"], 0);
        assert_eq!(prov["lastTs"], 4);
        assert_eq!(prov["routes"][0], "codex");
        // 无数据 → 空
        let empty_home = tmp("empty");
        assert_eq!(
            summary(&empty_home)["providers"].as_array().unwrap().len(),
            0
        );
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&empty_home);
    }

    #[test]
    fn rotation_bounds_files_and_summary_reads_both() {
        let home = tmp("rotate");
        let path = log_path(&home);
        std::fs::write(
            &path,
            "{\"provider_id\":\"old\",\"latency_ms\":10,\"ok\":true}\n",
        )
        .unwrap();
        rotate_if_needed(&path, 32, 32).unwrap();
        assert!(rotated_log_path(&path).exists());
        std::fs::write(
            &path,
            "{\"provider_id\":\"new\",\"latency_ms\":20,\"ok\":false}\n",
        )
        .unwrap();

        let providers = summary(&home)["providers"].as_array().unwrap().clone();
        assert_eq!(providers.len(), 2);
        assert!(providers
            .iter()
            .any(|provider| provider["providerId"] == "old"));
        assert!(providers
            .iter()
            .any(|provider| provider["providerId"] == "new"));
        let _ = std::fs::remove_dir_all(&home);
    }
}
