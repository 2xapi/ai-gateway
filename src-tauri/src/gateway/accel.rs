//! 网关加速线路装配：线路选择（accel_plan）、per-Key 凭证确保（ensure_line_cred）、
//! 407 判别（resolve_407_perkey）与降级记账。自 gateway.rs 按职责拆出（行为零变化）。

use std::time::Duration;

use crate::acclines::{AccLine, Cred};
use crate::server::AppState;

use super::forward::build_line_client;

// ── 阶段 4 加速装配(任务书 §五)+ 星图 任务 B:per-Key 凭证────────

/// 每账号节点凭证签发超过该时长视为过期,凭证确保段将重签(12h)。
const CRED_STALE_SECS: i64 = 12 * 3600;

/// 判断当前请求应走哪条加速线路(返回线路 + 凭证是否为 per-Key,供 407 判别):
/// - mode=custom → 自定义节点(全量走代理,凭证从 accel-credentials.json 注入;恒非 per-Key);
/// - mode=official → 按供应商 base_url 命中的官方线路:
///   有 per-Key 项且未降级 → 覆盖为该账号凭证;已降级 → None(直连,不再打节点);
///   无项但有 legacy → 保留共享凭证(老用户平滑);无项无 legacy → None(由凭证确保段尝试签发);
/// - mode=off / 未命中 → 直连(None)。
pub(super) fn accel_plan(
    state: &AppState,
    base_url: &str,
    api_key: &str,
) -> Option<(AccLine, bool)> {
    let cfg = state.accel.lock().unwrap_or_else(|p| p.into_inner());
    match cfg.mode.as_str() {
        "custom" => {
            let endpoint = cfg.custom_node.trim();
            if endpoint.is_empty() {
                None
            } else {
                Some((
                    AccLine {
                        id: "custom".into(),
                        name: "自定义节点".into(),
                        endpoint: endpoint.to_string(),
                        scope: Vec::new(),
                        priority: 0,
                        enabled: true,
                        credential: crate::acclines::load_credentials(&state.codex_home),
                    },
                    false,
                ))
            }
        }
        "official" => {
            let line = {
                let lines = state.health.lines.lock().unwrap_or_else(|p| p.into_inner());
                crate::acclines::match_line_healthy(base_url, &lines, &state.health).cloned()
            };
            let mut line = line?;
            let st = state.nodecreds.read().unwrap();
            match st.get_for_key(api_key) {
                Some(entry) if !entry.degraded_to_direct => {
                    // per-Key 覆盖:替换 acclines 注入的共享凭证
                    line.credential = Some(Cred {
                        user: entry.user.clone(),
                        pass: entry.pass.clone(),
                    });
                    Some((line, true))
                }
                Some(_) => None, // 已降级:本请求直接走直连
                None => {
                    if st.legacy_cred().is_some() {
                        Some((line, false)) // 老用户平滑:保留共享凭证兜底
                    } else {
                        None // 无凭证可用 → 直连(凭证确保段会尝试签发)
                    }
                }
            }
        }
        _ => None,
    }
}

/// 签发外呼统一限 5s(nodecreds 内建 10s,这里收紧为网关内联预算;超时视作不可达)。
async fn issue_timed(
    base: &str,
    api_key: &str,
) -> Result<crate::nodecreds::NodeCred, crate::nodecreds::IssueErr> {
    match tokio::time::timeout(
        Duration::from_secs(5),
        crate::nodecreds::issue_node_cred(base, api_key),
    )
    .await
    {
        Ok(r) => r,
        Err(_) => Err(crate::nodecreds::IssueErr::Unreachable(
            "签发超时(5s)".into(),
        )),
    }
}

/// 记降级:store 该 key 项 degraded_to_direct=true(快照若带配额数字一并回写)+ 落盘。
/// 无该项(如 legacy 用户)则无项可记,no-op。pass 永不进日志。
fn mark_degraded(state: &AppState, api_key: &str, snap: Option<&crate::nodecreds::QuotaSnapshot>) {
    let mut st = state.nodecreds.write().unwrap();
    if let Some(e) = st.creds.get_mut(&crate::nodecreds::hash_key(api_key)) {
        e.degraded_to_direct = true;
        if let Some(s) = snap {
            if let Some(u) = s.quota_used_bytes {
                e.quota_used_bytes = u;
            }
            if let Some(t) = s.quota_total_bytes {
                e.quota_total_bytes = t;
            }
        }
        let _ = crate::nodecreds::save_store(&state.codex_home, &st);
    }
}

/// 凭证确保段(星图 任务 B2):official 命中线但 store 无该 key 项(或签发超 12h)→
/// 同步签发(5s 超时,no_proxy):
/// - Ok → set_for_key + save_store + 覆盖凭证(per-Key);
/// - Err(Unreachable) → 本请求跳线直连(不报错,日志);legacy 凭证线保留(老用户平滑);
/// - Err(QuotaFull/KeyInvalid) → 跳线直连 + 记 degraded。
pub(super) async fn ensure_line_cred(
    state: &AppState,
    line: Option<(AccLine, bool)>,
    base_url: &str,
    api_key: &str,
) -> Option<(AccLine, bool)> {
    let mode = {
        let cfg = state.accel.lock().unwrap_or_else(|p| p.into_inner());
        cfg.mode.clone()
    };
    if mode != "official" || api_key.trim().is_empty() {
        return line;
    }
    // 快照判定:该 key 项是否降级 / 是否需要(重)签发
    let (degraded, needs_issue) = {
        let st = state.nodecreds.read().unwrap();
        match st.get_for_key(api_key) {
            Some(e) if e.degraded_to_direct => (true, false),
            Some(e) => (
                false,
                chrono::Utc::now().timestamp() - e.issued_at > CRED_STALE_SECS,
            ),
            None => (false, true),
        }
    };
    if degraded {
        return None; // 已降级:直连,不再签发
    }
    if !needs_issue {
        return line; // 新鲜项:accel_plan 已完成 per-Key 覆盖
    }
    // 无线路时再取一次命中线(accel_plan 的 None 含「无项无 legacy」可签发场景)
    let base_line = match &line {
        Some((l, _)) => Some(l.clone()),
        None => {
            let lines = state.health.lines.lock().unwrap_or_else(|p| p.into_inner());
            crate::acclines::match_line_healthy(base_url, &lines, &state.health).cloned()
        }
    };
    let Some(mut l) = base_line else {
        return None; // 未命中官方线路:直连,不签发
    };
    match issue_timed(&crate::server::issue_base(), api_key).await {
        Ok(cred) => {
            {
                let mut st = state.nodecreds.write().unwrap();
                st.set_for_key(api_key, cred.clone());
                let _ = crate::nodecreds::save_store(&state.codex_home, &st);
            }
            eprintln!("[GW] 每账号节点凭证已签发并落盘");
            l.credential = Some(Cred {
                user: cred.user,
                pass: cred.pass,
            });
            Some((l, true))
        }
        Err(crate::nodecreds::IssueErr::Unreachable(e)) => {
            eprintln!("[GW] 节点凭证签发不可达({e}),本请求跳线直连");
            match line {
                Some((l, pk)) if !pk => Some((l, pk)), // legacy 共享凭证线保留
                _ => None,
            }
        }
        Err(crate::nodecreds::IssueErr::QuotaFull(snap)) => {
            eprintln!("[GW] 节点凭证签发:配额满,该 Key 记降级并本请求直连");
            mark_degraded(state, api_key, snap.as_ref());
            None
        }
        Err(crate::nodecreds::IssueErr::KeyInvalid) => {
            eprintln!("[GW] 节点凭证签发:Key 无效,该 Key 记降级并本请求直连");
            mark_degraded(state, api_key, None);
            None
        }
    }
}

/// 407 判别的结果:重签成功(新凭证 line client)/本请求直连/凭证无效(维持 502)。
pub(super) enum Resolve407 {
    NewClient(reqwest::Client),
    Direct,
    Invalid,
}

/// per-Key 凭证的 407 判别(星图 任务 B3;安全前提同换线重试:407 在隧道握手阶段,
/// 上游未收到任何字节,故重试/换直连都不会重复副作用):
/// - 重签 Ok → 新凭证重建 line_client,由调用方重试原请求一次;
/// - Err(QuotaFull) → store 该 key degraded_to_direct=true + 落盘,本请求直连;
/// - Err(KeyInvalid) → 维持 502「节点凭证无效」(不绕过用户指定线路);
/// - Err(Unreachable) → 本请求直连。
///
/// legacy/custom 凭证的 407 不进本函数(调用方维持原 502 行为)。
pub(super) async fn resolve_407_perkey(
    state: &AppState,
    api_key: &str,
    line: &AccLine,
    timeout: Duration,
) -> Resolve407 {
    eprintln!("[GW] 407 判别:重签每账号凭证");
    match issue_timed(&crate::server::issue_base(), api_key).await {
        Ok(cred) => {
            {
                let mut st = state.nodecreds.write().unwrap();
                st.set_for_key(api_key, cred.clone());
                let _ = crate::nodecreds::save_store(&state.codex_home, &st);
            }
            let l = AccLine {
                credential: Some(Cred {
                    user: cred.user,
                    pass: cred.pass,
                }),
                ..line.clone()
            };
            match build_line_client(&l, timeout) {
                Ok(c) => Resolve407::NewClient(c),
                Err(e) => {
                    eprintln!("[GW] 重签后建线失败({e}),本请求直连");
                    Resolve407::Direct
                }
            }
        }
        Err(crate::nodecreds::IssueErr::QuotaFull(snap)) => {
            mark_degraded(state, api_key, snap.as_ref());
            eprintln!("[GW] 407 判别:配额满,该 Key 降级直连并落盘");
            Resolve407::Direct
        }
        Err(crate::nodecreds::IssueErr::KeyInvalid) => {
            eprintln!("[GW] 407 判别:Key 无效,维持 502(不绕过用户指定线路)");
            Resolve407::Invalid
        }
        Err(crate::nodecreds::IssueErr::Unreachable(e)) => {
            eprintln!("[GW] 407 判别:节点不可达({e}),本请求直连");
            Resolve407::Direct
        }
    }
}
