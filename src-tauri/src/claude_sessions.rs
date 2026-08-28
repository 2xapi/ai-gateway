//! Claude 会话历史(R2 批次:占位 → 真实;纯只读)。
//!
//! 扫 `~/.claude/projects/<目录路径转义>/<uuid>.jsonl`:每文件一个会话。
//! 契约:GET /api/claude/sessions?page&size → {ok:true, data:{total, items:[{id,title,cwd,updatedAt}]}}
//!   - id = 文件名(uuid,去 .jsonl 后缀)
//!   - title = 首条 user 消息文本(只读前 TITLE_SCAN_LINES 行,不全量解析;找不到 → "(无标题)")
//!   - cwd = 目录名反转义(前导 `-` 是根,后续 `-` 分段)
//!   - updatedAt = 文件 mtime(毫秒),倒序分页
//!
//! `~/.claude` 缺失 → {total:0, items:[]},不报错。
//!
//! 安全约定:本模块只读 ~/.claude,绝无任何写操作(Claude 会话文件归 CLI 自己管)。

use serde_json::{json, Value};
use std::io::BufRead;
use std::path::{Path, PathBuf};

/// title 提取最多读多少行 jsonl(大文件不全量解析,规格 §A1)。
const TITLE_SCAN_LINES: usize = 50;
/// title 最大字符数(超出截断加省略号;按字符截,不切断 UTF-8)。
const TITLE_MAX_CHARS: usize = 80;
/// 找不到可用 user 消息时的兜底标题。
const NO_TITLE: &str = "(无标题)";

/// 目录名反转义:Claude CLI 把 cwd 转成 `-Users-wenkezhi-Documents-xxx` 形式。
/// 规则:前导 `-` 是根 `/`,后续 `-` 分段;空段过滤(原路径含非 ASCII 字符时
/// CLI 会把它们压成连串 `-`,不可逆,过滤后展示最接近的真实路径)。
fn unescape_cwd(dir_name: &str) -> String {
    let body = dir_name.strip_prefix('-').unwrap_or(dir_name);
    let parts: Vec<&str> = body.split('-').filter(|s| !s.is_empty()).collect();
    format!("/{}", parts.join("/"))
}

/// message.content 提取文本:字符串直取;数组取各 text 块拼接(忽略 tool_result 等非文本块)。
fn text_from_content(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// 单行 jsonl 是否可作为 title 来源:
/// type=="user" 且非 isMeta,文本非空,且非命令/系统注入行(<command-name> 等,真机样例所见)。
fn title_from_line(v: &Value) -> Option<String> {
    if v.get("type").and_then(|t| t.as_str()) != Some("user") {
        return None;
    }
    if v.get("isMeta").and_then(|m| m.as_bool()).unwrap_or(false) {
        return None;
    }
    let text = v
        .get("message")
        .and_then(|m| m.get("content"))
        .map(text_from_content)
        .unwrap_or_default();
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return None;
    }
    // 真机样例:CLI 注入的命令行/提示行不作标题
    if flat.starts_with("<command-name>")
        || flat.starts_with("<local-command")
        || flat.starts_with("Caveat:")
    {
        return None;
    }
    Some(flat)
}

/// 按字符数截断(UTF-8 安全),超出加省略号。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// 读 jsonl 前 TITLE_SCAN_LINES 行找 title;找不到(空文件/无 user 消息/解析全败)→ None。
fn extract_title(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    for line in std::io::BufReader::new(file).lines().take(TITLE_SCAN_LINES) {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            if let Some(t) = title_from_line(&v) {
                return Some(truncate_chars(&t, TITLE_MAX_CHARS));
            }
        }
    }
    None
}

/// 未分页的原始条目(title 延后到分页切片再取:大目录只解析当页文件的头部,省 IO)。
struct RawSession {
    id: String,
    cwd: String,
    updated_ms: i64,
    path: PathBuf,
}

fn mtime_ms(path: &Path) -> i64 {
    path.metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 核心:扫 projects/*/*.jsonl → {total, items}。任何缺失/不可读都静默归空,不报错。
pub fn list_sessions(claude_home: &Path, page: usize, size: usize) -> Value {
    let projects = claude_home.join("projects");
    let mut raw: Vec<RawSession> = Vec::new();
    if let Ok(dirs) = std::fs::read_dir(&projects) {
        for dir in dirs.flatten() {
            if !dir.path().is_dir() {
                continue;
            }
            let dir_name = dir.file_name().to_string_lossy().to_string();
            let cwd = unescape_cwd(&dir_name);
            if let Ok(files) = std::fs::read_dir(dir.path()) {
                for f in files.flatten() {
                    let fname = f.file_name().to_string_lossy().to_string();
                    if !fname.ends_with(".jsonl") {
                        continue;
                    }
                    raw.push(RawSession {
                        id: fname.trim_end_matches(".jsonl").to_string(),
                        cwd: cwd.clone(),
                        updated_ms: mtime_ms(&f.path()),
                        path: f.path(),
                    });
                }
            }
        }
    }
    // updatedAt 倒序;同值按 id 倒序稳定
    raw.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms).then(b.id.cmp(&a.id)));
    let total = raw.len();
    let page = page.max(1);
    let size = size.clamp(1, 200);
    let start = (page - 1) * size;
    let items: Vec<Value> = raw
        .into_iter()
        .skip(start)
        .take(size)
        .map(|s| {
            json!({
                "id": s.id,
                "title": extract_title(&s.path).unwrap_or_else(|| NO_TITLE.into()),
                "cwd": s.cwd,
                "updatedAt": s.updated_ms,
            })
        })
        .collect();
    json!({ "total": total, "items": items })
}

/// 本机 ~/.claude(HOME 环境变量;缺 HOME → 空 PathBuf,自然归 {total:0})。
fn claude_home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claude")
}

/// GET /api/claude/sessions?page&size(路由注册在 server.rs)。
/// 04 统一信封:{ok:true, data:{total, items}};~/.claude 不存在也 200 空列表。
pub async fn handle_list(query: axum::extract::Query<Value>) -> axum::Json<Value> {
    let page = query
        .get("page")
        .and_then(|v| v.as_str())
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1);
    let size = query
        .get("size")
        .and_then(|v| v.as_str())
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(50);
    axum::Json(json!({ "ok": true, "data": list_sessions(&claude_home(), page, size) }))
}

// ── 单测(tempdir 假 ~/.claude/projects 结构,绝不碰真实 ~/.claude)──

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    fn sandbox(label: &str) -> PathBuf {
        let n = N.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "2xapi-claude-sess-{label}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    /// 造一个会话文件(返回其路径);mtime 由调用方以 sleep 拉开(APFS 纳秒精度,20ms 足够区分)。
    fn write_session(root: &Path, dir: &str, id: &str, lines: &[&str]) -> PathBuf {
        let d = root.join("projects").join(dir);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join(format!("{id}.jsonl"));
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        p
    }

    /// ① 排序 + 分页:3 个会话 mtime 递增(旧→新),断言倒序、分页切片、total 正确。
    #[test]
    fn list_orders_by_mtime_desc_and_paginates() {
        let root = sandbox("order");
        let user = r#"{"type":"user","message":{"role":"user","content":"hi"}}"#;
        write_session(&root, "-Users-u-p1", "aaa-old", &[user]);
        std::thread::sleep(std::time::Duration::from_millis(25));
        write_session(&root, "-Users-u-p1", "bbb-mid", &[user]);
        std::thread::sleep(std::time::Duration::from_millis(25));
        write_session(&root, "-Users-u-p2", "ccc-new", &[user]);

        let r = list_sessions(&root, 1, 2);
        assert_eq!(r["total"], 3);
        let items = r["items"].as_array().unwrap();
        assert_eq!(items.len(), 2, "size=2 首页两条");
        assert_eq!(items[0]["id"], "ccc-new", "最新在前");
        assert_eq!(items[1]["id"], "bbb-mid");
        assert_eq!(items[0]["title"], "hi");

        let r2 = list_sessions(&root, 2, 2);
        assert_eq!(r2["total"], 3);
        let items2 = r2["items"].as_array().unwrap();
        assert_eq!(items2.len(), 1);
        assert_eq!(items2[0]["id"], "aaa-old", "次页最旧");
        // 越界页 → 空列表不报错
        let r3 = list_sessions(&root, 9, 50);
        assert_eq!(r3["total"], 3);
        assert_eq!(r3["items"].as_array().unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ② title 提取:字符串 content / 数组 content(取 text 块,忽略 tool_result)两形态;
    ///    跳过 isMeta 与命令注入行;无 user 消息 → "(无标题)";超长截断。
    #[test]
    fn title_extraction_string_and_array_content() {
        let root = sandbox("title");
        // 字符串形态:首行 queue-operation(非 user)先出现,应跳过取到后面 user
        write_session(
            &root,
            "-Users-u-p",
            "s-str",
            &[
                r#"{"type":"queue-operation","operation":"dequeue"}"#,
                r#"{"type":"user","message":{"role":"user","content":"帮我看下这个报错怎么修"}}"#,
            ],
        );
        // 数组形态:text 块 + tool_result 块混排,只取文本
        write_session(
            &root,
            "-Users-u-p",
            "s-arr",
            &[
                r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"<command-name>/clear</command-name>"}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ignored"},{"type":"text","text":"重构登录模块"}]}}"#,
            ],
        );
        // 无 user 消息 → 兜底
        write_session(
            &root,
            "-Users-u-p",
            "s-none",
            &[r#"{"type":"assistant","message":{"role":"assistant","content":"..."}}"#],
        );
        // 首个 user 是命令注入 → 跳过,取下一条真消息
        write_session(
            &root,
            "-Users-u-p",
            "s-cmd",
            &[
                r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"<command-message>compact</command-message>"}}"#,
                r#"{"type":"user","message":{"role":"user","content":"压缩上下文后的第二个问题"}}"#,
            ],
        );
        // 超长 → 截断加省略号(80 字符 + …)
        let long = "很".repeat(100);
        write_session(
            &root,
            "-Users-u-p",
            "s-long",
            &[&format!(
                r#"{{"type":"user","message":{{"role":"user","content":"{long}"}}}}"#
            )],
        );

        let r = list_sessions(&root, 1, 50);
        let title = |id: &str| -> String {
            let items = r["items"].as_array().unwrap();
            let it = items.iter().find(|x| x["id"] == id).unwrap();
            it["title"].as_str().unwrap().to_string()
        };
        assert_eq!(title("s-str"), "帮我看下这个报错怎么修");
        assert_eq!(title("s-arr"), "重构登录模块", "数组 content 只取 text 块");
        assert_eq!(title("s-none"), "(无标题)");
        assert_eq!(
            title("s-cmd"),
            "压缩上下文后的第二个问题",
            "命令注入行应跳过"
        );
        let t = title("s-long");
        assert!(t.chars().count() == 81 && t.ends_with('…'), "超长截断: {t}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ③ 目录反转义:常规多级、连串 `-`(空段过滤)、无前导 `-` 容错。
    #[test]
    fn unescape_cwd_variants() {
        assert_eq!(
            unescape_cwd("-Users-wenkezhi-Documents-xxx"),
            "/Users/wenkezhi/Documents/xxx"
        );
        assert_eq!(unescape_cwd("-private-tmp"), "/private/tmp");
        // 原路径含非 ASCII 被压成连串 `-`(如 -Users-x-Documents-sub2api-----)→ 空段过滤
        assert_eq!(
            unescape_cwd("-Users-x-Documents-sub2api-----"),
            "/Users/x/Documents/sub2api"
        );
        // 无前导 `-`(非常规,容错不炸)
        assert_eq!(unescape_cwd("Users-x"), "/Users/x");
    }

    /// ④ 空目录容错:~/.claude 不存在 / projects 存在但空 / 混入非 jsonl 文件与散文件。
    #[test]
    fn missing_or_empty_dirs_return_empty_without_error() {
        // ~/.claude 整个不存在
        let gone = sandbox("gone").join("never");
        let r = list_sessions(&gone, 1, 50);
        assert_eq!(r["total"], 0);
        assert_eq!(r["items"].as_array().unwrap().len(), 0);
        // projects 存在但空
        let root = sandbox("empty");
        std::fs::create_dir_all(root.join("projects")).unwrap();
        let r = list_sessions(&root, 1, 50);
        assert_eq!(r["total"], 0);
        // 混入:非 jsonl 文件跳过;projects 直下散文件跳过;坏行 jsonl 仍出条目(title 兜底)
        let dir = root.join("projects/-Users-u-p");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("readme.txt"), "noise").unwrap();
        std::fs::write(dir.join("ok.jsonl"), "not-json-at-all\n").unwrap();
        std::fs::write(root.join("projects/loose.jsonl"), "{}").unwrap();
        let r = list_sessions(&root, 1, 50);
        let items = r["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "只收 ok.jsonl;txt 与散文件跳过");
        assert_eq!(items[0]["id"], "ok");
        assert_eq!(items[0]["title"], "(无标题)", "坏行 jsonl 不崩,title 兜底");
        let _ = std::fs::remove_dir_all(&root);
    }
}
