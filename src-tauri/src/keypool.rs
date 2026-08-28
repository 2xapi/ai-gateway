//! Key 资源池(超融合 A 线一期 §1,方案 v1.0):
//! `Provider.keys[]` 多 Key 池 + 轮询 + 故障切换(429/5xx/超时冷却换 Key)。
//!
//! 红线:老单 Key(keys 空)行为零变化——`apply` 直接原样返回,不进池不打点;
//! 池状态仅存在于内存(cursor/cooldowns),providers.json 持久层只多一个可选 keys 字段。
//! 失败上报(dispatch 拿到 429/5xx/超时后调 mark_failure)→ 该 Key 冷却 60s,
//! 期间轮询跳过;全冷却 → 返回最早解冻的 Key(不阻塞流量,宁试勿拒)。

use crate::providers::Provider;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 单 Key 冷却时长(429 限流常规窗口+余量;5xx/超时多为瞬时,60s 足够翻篇)。
pub const COOLDOWN: Duration = Duration::from_secs(60);

struct PoolEntry {
    /// 池内 key 快照(与 cooldown_until 同序;keys 变更时 pick 侧自动对齐)
    keys: Vec<String>,
    cursor: usize,
    /// None=未冷却
    cooldown_until: Vec<Option<Instant>>,
}

#[derive(Default)]
pub struct KeyPool {
    inner: Mutex<HashMap<String, PoolEntry>>,
}

/// 供应商的有效 Key 列表:keys 非空用 keys,否则回退单 api_key(兼容迁移,不写回文件)。
pub fn effective_keys(p: &Provider) -> Vec<String> {
    let ks: Vec<String> = p
        .keys
        .iter()
        .filter(|k| !k.trim().is_empty())
        .cloned()
        .collect();
    if ks.is_empty() {
        vec![p.api_key.clone()]
    } else {
        ks
    }
}

impl KeyPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// 选 Key:单 Key 直返(零行为变化);多 Key 轮询跳过冷却中,全冷却取最早解冻。
    pub fn pick(&self, p: &Provider) -> String {
        let keys = effective_keys(p);
        if keys.len() <= 1 {
            return keys.into_iter().next().unwrap_or_default();
        }
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let entry = map.entry(p.id.clone()).or_insert_with(|| PoolEntry {
            keys: keys.clone(),
            cursor: 0,
            cooldown_until: vec![None; keys.len()],
        });
        // keys 变更(增删 Key 后保存)→ 对齐快照:保留同 key 的冷却,新 key 无冷却
        if entry.keys != keys {
            let old: HashMap<&str, Instant> = entry
                .keys
                .iter()
                .zip(entry.cooldown_until.iter())
                .filter_map(|(k, c)| c.map(|t| (k.as_str(), t)))
                .collect();
            let now = Instant::now();
            entry.cooldown_until = keys
                .iter()
                .map(|k| old.get(k.as_str()).copied().filter(|t| *t > now))
                .collect();
            entry.keys = keys.clone();
        }
        let now = Instant::now();
        // 全冷却 → 最早解冻的
        if entry
            .cooldown_until
            .iter()
            .all(|c| c.map(|t| t > now).unwrap_or(false))
        {
            let best = entry
                .cooldown_until
                .iter()
                .enumerate()
                .filter_map(|(i, c)| c.map(|t| (i, t)))
                .min_by_key(|(_, t)| *t)
                .map(|(i, _)| i)
                .unwrap_or(0);
            return keys[best].clone();
        }
        // 从 cursor 起找第一个未冷却
        for off in 0..keys.len() {
            let idx = (entry.cursor + off) % keys.len();
            let cooled = entry.cooldown_until[idx].map(|t| t > now).unwrap_or(false);
            if !cooled {
                entry.cursor = (idx + 1) % keys.len();
                return keys[idx].clone();
            }
        }
        keys[entry.cursor % keys.len()].clone()
    }

    /// 标失败(429/5xx/超时):冷却该 Key(按值匹配,池内重复 key 一并冷却)。
    pub fn mark_failure(&self, provider_id: &str, key: &str) {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(entry) = map.get_mut(provider_id) else {
            return;
        };
        let until = Instant::now() + COOLDOWN;
        for (i, k) in entry.keys.iter().enumerate() {
            if k == key {
                if let Some(slot) = entry.cooldown_until.get_mut(i) {
                    *slot = Some(until);
                }
            }
        }
    }

    /// 标成功:清该供应商全部冷却(成功说明上游恢复)。
    pub fn mark_success(&self, provider_id: &str) {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(entry) = map.get_mut(provider_id) {
            for slot in entry.cooldown_until.iter_mut() {
                *slot = None;
            }
        }
    }
}

/// dispatch 注入点:多 Key 时替换 api_key 为池选 Key 的 provider clone;单 Key 原样返回。
pub fn apply(pool: &KeyPool, provider: Provider) -> Provider {
    let keys = effective_keys(&provider);
    if keys.len() <= 1 {
        return provider;
    }
    let mut p = provider;
    p.api_key = pool.pick(&p);
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prov(id: &str, keys: &[&str]) -> Provider {
        Provider {
            id: id.to_string(),
            api_key: keys[0].to_string(),
            keys: keys.iter().map(|k| k.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn single_key_passthrough() {
        let p = Provider {
            id: "x".into(),
            api_key: "sk-single".into(),
            keys: vec![],
            ..Default::default()
        };
        let pool = KeyPool::new();
        assert_eq!(pool.pick(&p), "sk-single", "单 Key 直返");
        assert_eq!(effective_keys(&p), vec!["sk-single"], "keys 空回退 api_key");
        // apply 原样返回(api_key 不变)
        let p2 = apply(&pool, p);
        assert_eq!(p2.api_key, "sk-single");
    }

    #[test]
    fn multi_key_round_robin() {
        let p = prov("m", &["k1", "k2", "k3"]);
        let pool = KeyPool::new();
        let seq: Vec<String> = (0..6).map(|_| pool.pick(&p)).collect();
        assert_eq!(seq, vec!["k1", "k2", "k3", "k1", "k2", "k3"], "轮询有序");
        let p2 = apply(&pool, prov("m", &["k1", "k2", "k3"]));
        assert!(
            ["k1", "k2", "k3"].contains(&p2.api_key.as_str()),
            "apply 替换为池内 Key"
        );
    }

    #[test]
    fn cooldown_skips_and_recovers() {
        let p = prov("c", &["k1", "k2"]);
        let pool = KeyPool::new();
        assert_eq!(pool.pick(&p), "k1");
        pool.mark_failure("c", "k1");
        assert_eq!(pool.pick(&p), "k2", "冷却中的 key 被跳过");
        assert_eq!(pool.pick(&p), "k2", "持续跳过");
        pool.mark_success("c");
        // 清冷却后按 cursor 轮转(前两次 pick(k2) 已把 cursor 推回 0)→ k1、k2 交替
        assert_eq!(pool.pick(&p), "k1", "冷却清除后按 cursor 轮转");
        assert_eq!(pool.pick(&p), "k2", "轮询继续交替");
    }

    #[test]
    fn all_cooled_returns_earliest() {
        let p = prov("a", &["k1", "k2"]);
        let pool = KeyPool::new();
        let _ = pool.pick(&p);
        {
            let mut map = pool.inner.lock().unwrap_or_else(|p| p.into_inner());
            let e = map.get_mut("a").unwrap();
            e.cooldown_until[0] = Some(Instant::now() + Duration::from_secs(50));
            e.cooldown_until[1] = Some(Instant::now() + Duration::from_secs(10));
        }
        assert_eq!(pool.pick(&p), "k2", "全冷却取最早解冻");
    }

    #[test]
    fn key_size_change_keeps_cooldowns() {
        let p2 = prov("r", &["k1", "k2"]);
        let pool = KeyPool::new();
        let _ = pool.pick(&p2);
        pool.mark_failure("r", "k1");
        // keys 增至 3(保存新 Key 后):k1 冷却保留,k3 无冷却
        let p3 = prov("r", &["k1", "k2", "k3"]);
        assert_eq!(pool.pick(&p3), "k2", "对齐后仍跳过冷却中的 k1");
        assert_eq!(pool.pick(&p3), "k3", "新 key 参与轮询");
        {
            let map = pool.inner.lock().unwrap_or_else(|p| p.into_inner());
            let e = map.get("r").unwrap();
            assert_eq!(e.cooldown_until.len(), 3, "冷却表随池伸缩");
        }
    }
}
