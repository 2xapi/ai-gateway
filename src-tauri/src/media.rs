//! 媒体服务本体(超融合 A 线二期,《超融合与生态中心开发方案.md》§A 线二期 +
//! 《媒体服务A段定案.md》M6):原件暂存 + URL 回传。
//!
//! 形态 A 边界(M6 定案):本地 Console 无公网,上游拉不动回环 URL——本模块的 URL
//! 仅作**管内引用**(本机插件/挂载点间传递);上游要原件时由后续管线读取暂存转 base64。
//!
//! 设计约束:
//! - 零 AppState 字段:存储根一律从 `state.codex_home` 派生(`{codex_home}/media`),新增路由不改状态结构。
//! - id = uuid v4(128 bit,URL 即凭证,不可枚举);文件名 `{id}.{ext}`。
//! - MIME 白名单 + 魔数嗅探双验证(防伪装),分型大小上限(图 20MB/音频 25MB/PDF 50MB/视频 100MB)。
//! - 索引 `index.json` 原子写(临时文件→rename);配额默认 500MB,超限按 last_access 最旧逐出(LRU)。

use axum::extract::{Path as AxumPath, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};

/// 配额默认值(500MB;M6 定案倾向案:持久+LRU)。
pub const DEFAULT_QUOTA_BYTES: u64 = 500 * 1024 * 1024;

const MB: u64 = 1024 * 1024;

// ── MIME 白名单与魔数嗅探 ──────────────────────────────────────

struct MediaKind {
    mime: &'static str,
    ext: &'static str,
    max_bytes: u64,
    magic: fn(&[u8]) -> bool,
}

const KINDS: &[MediaKind] = &[
    MediaKind {
        mime: "image/jpeg",
        ext: "jpg",
        max_bytes: 20 * MB,
        magic: |d| d.starts_with(&[0xFF, 0xD8, 0xFF]),
    },
    MediaKind {
        mime: "image/png",
        ext: "png",
        max_bytes: 20 * MB,
        magic: |d| d.starts_with(&[0x89, b'P', b'N', b'G']),
    },
    MediaKind {
        mime: "image/gif",
        ext: "gif",
        max_bytes: 20 * MB,
        magic: |d| d.starts_with(b"GIF8"),
    },
    // RIFF 容器双签名(WEBP/WAV)按偏移 8 分流
    MediaKind {
        mime: "image/webp",
        ext: "webp",
        max_bytes: 20 * MB,
        magic: |d| riff_at(d, b"WEBP"),
    },
    MediaKind {
        mime: "audio/wav",
        ext: "wav",
        max_bytes: 25 * MB,
        magic: |d| riff_at(d, b"WAVE"),
    },
    // MPEG 音频:ID3 头或帧同步字节(0xFF Ex/Fx)
    MediaKind {
        mime: "audio/mpeg",
        ext: "mp3",
        max_bytes: 25 * MB,
        magic: |d| d.starts_with(b"ID3") || (d.len() >= 2 && d[0] == 0xFF && d[1] & 0xE0 == 0xE0),
    },
    MediaKind {
        mime: "video/mp4",
        ext: "mp4",
        max_bytes: 100 * MB,
        magic: |d| d.len() >= 12 && &d[4..8] == b"ftyp",
    },
    MediaKind {
        mime: "video/webm",
        ext: "webm",
        max_bytes: 100 * MB,
        magic: |d| d.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]),
    },
    MediaKind {
        mime: "application/pdf",
        ext: "pdf",
        max_bytes: 50 * MB,
        magic: |d| d.starts_with(b"%PDF"),
    },
];

/// RIFF 容器:RIFF+4 字节长度+格式签名。
fn riff_at(d: &[u8], sig: &[u8; 4]) -> bool {
    d.len() >= 12 && d.starts_with(b"RIFF") && &d[8..12] == sig
}

/// 魔数嗅探:返回命中的白名单型别(未命中=None)。
fn sniff(data: &[u8]) -> Option<&'static MediaKind> {
    KINDS.iter().find(|k| (k.magic)(data))
}

// ── 索引(磁盘即真相,原子写)─────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaItem {
    pub id: String,
    pub mime: String,
    pub ext: String,
    pub size: u64,
    pub created_at: i64,
    pub last_access: i64,
    /// 来源标记(如 "upload"/"pipeline"/插件 id),排查用。
    #[serde(default)]
    pub origin: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct MediaIndex {
    #[serde(default)]
    schema_version: i64,
    #[serde(default)]
    quota_bytes: u64,
    items: Vec<MediaItem>,
}

pub fn media_root(codex_home: &Path) -> PathBuf {
    codex_home.join("media")
}

fn item_path(root: &Path, item: &MediaItem) -> PathBuf {
    root.join(format!("{}.{}", item.id, item.ext))
}

fn load_index(root: &Path) -> MediaIndex {
    let mut idx: MediaIndex = std::fs::read_to_string(root.join("index.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    if idx.schema_version == 0 {
        idx.schema_version = 1;
    }
    if idx.quota_bytes == 0 {
        idx.quota_bytes = DEFAULT_QUOTA_BYTES;
    }
    idx
}

fn save_index(root: &Path, idx: &MediaIndex) -> Result<(), String> {
    let raw = serde_json::to_string_pretty(idx).map_err(|e| format!("序列化失败: {e}"))?;
    let tmp = root.join("index.json.tmp");
    std::fs::write(&tmp, &raw).map_err(|e| format!("写临时文件失败: {e}"))?;
    std::fs::rename(&tmp, root.join("index.json")).map_err(|e| format!("重命名失败: {e}"))?;
    Ok(())
}

/// 上传核心:嗅探→上限→落盘→入索引→配额逐出。返回新建条目。
/// declared_mime 为空时以嗅探结果为准(宽松);非空且与嗅探不符 → E_MEDIA_MIME(防伪装)。
pub(crate) fn store_upload(
    codex_home: &Path,
    data: &[u8],
    declared_mime: &str,
    origin: &str,
) -> Result<MediaItem, (&'static str, String)> {
    let root = media_root(codex_home);
    std::fs::create_dir_all(&root)
        .map_err(|e| ("E_MEDIA_STORE", format!("创建暂存目录失败: {e}")))?;
    let kind = sniff(data).ok_or((
        "E_MEDIA_MIME",
        "无法识别媒体类型(不在白名单或数据损坏)".to_string(),
    ))?;
    if !declared_mime.is_empty() && declared_mime != kind.mime {
        return Err((
            "E_MEDIA_MIME",
            format!("声明的 {} 与实际内容 {} 不符", declared_mime, kind.mime),
        ));
    }
    if data.len() as u64 > kind.max_bytes {
        return Err((
            "E_MEDIA_TOO_LARGE",
            format!("{} 超过上限 {} MB", kind.mime, kind.max_bytes / MB),
        ));
    }

    let now = chrono::Utc::now().timestamp();
    let item = MediaItem {
        id: uuid::Uuid::new_v4().simple().to_string(),
        mime: kind.mime.to_string(),
        ext: kind.ext.to_string(),
        size: data.len() as u64,
        created_at: now,
        last_access: now,
        origin: origin.chars().take(64).collect(),
    };
    let path = item_path(&root, &item);
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, data).map_err(|e| ("E_MEDIA_STORE", format!("写暂存失败: {e}")))?;
    std::fs::rename(&tmp, &path).map_err(|e| ("E_MEDIA_STORE", format!("落盘失败: {e}")))?;

    let mut idx = load_index(&root);
    idx.items.push(item.clone());
    evict_to_quota(&root, &mut idx);
    save_index(&root, &idx).map_err(|e| ("E_MEDIA_STORE", e))?;
    Ok(item)
}

/// LRU 逐出:总量超配额时按 last_access 最旧先删(文件+条目);逐出失败不阻塞(条目仍删)。
/// 恒保留至少一件(单件超配额不逐出——上传时分型上限已拦截畸形件)。
fn evict_to_quota(root: &Path, idx: &mut MediaIndex) {
    let total: u64 = idx.items.iter().map(|i| i.size).sum();
    if total <= idx.quota_bytes {
        return;
    }
    idx.items.sort_by_key(|i| i.last_access);
    let mut acc = total;
    let mut evicted = 0;
    while acc > idx.quota_bytes && idx.items.len() > evicted + 1 {
        let victim = &idx.items[evicted];
        let _ = std::fs::remove_file(item_path(root, victim));
        acc = acc.saturating_sub(victim.size);
        evicted += 1;
    }
    idx.items.drain(0..evicted);
}

fn find_by_file<'a>(idx: &'a MediaIndex, id: &str, ext: &str) -> Option<&'a MediaItem> {
    idx.items.iter().find(|i| i.id == id && i.ext == ext)
}

// ── HTTP handlers(build_router 注册;信封=ok_env/err_env)──────────

#[derive(Deserialize)]
pub struct UploadBody {
    #[serde(default)]
    mime: String,
    data_b64: String,
    #[serde(default)]
    origin: String,
}

pub async fn handle_upload(
    State(s): State<std::sync::Arc<crate::server::AppState>>,
    Json(body): Json<UploadBody>,
) -> Response {
    let data = match b64_decode(&body.data_b64) {
        Ok(d) => d,
        Err(e) => return crate::server::err_env(StatusCode::BAD_REQUEST, "E_MEDIA_B64", &e, None),
    };
    match store_upload(&s.codex_home, &data, body.mime.trim(), body.origin.trim()) {
        Ok(item) => crate::server::ok_env(json!({
            "id": item.id, "mime": item.mime, "size": item.size,
            "url": format!("/media/{}.{}", item.id, item.ext),
        })),
        Err((code, msg)) => {
            let status = match code {
                "E_MEDIA_TOO_LARGE" => StatusCode::PAYLOAD_TOO_LARGE,
                "E_MEDIA_MIME" => StatusCode::UNSUPPORTED_MEDIA_TYPE,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            crate::server::err_env(status, code, &msg, None)
        }
    }
}

pub async fn handle_list(State(s): State<std::sync::Arc<crate::server::AppState>>) -> Response {
    let idx = load_index(&media_root(&s.codex_home));
    crate::server::ok_env(json!({
        "quotaBytes": idx.quota_bytes,
        "totalBytes": idx.items.iter().map(|i| i.size).sum::<u64>(),
        "items": idx.items,
    }))
}

pub async fn handle_delete(
    State(s): State<std::sync::Arc<crate::server::AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let root = media_root(&s.codex_home);
    let mut idx = load_index(&root);
    let Some(pos) = idx.items.iter().position(|i| i.id == id) else {
        return crate::server::err_env(
            StatusCode::NOT_FOUND,
            "E_MEDIA_NOT_FOUND",
            "媒体件不存在",
            None,
        );
    };
    let item = idx.items.remove(pos);
    let _ = std::fs::remove_file(item_path(&root, &item));
    if let Err(e) = save_index(&root, &idx) {
        return crate::server::err_env(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_MEDIA_STORE",
            &e,
            None,
        );
    }
    crate::server::ok_env(json!({ "id": id, "deleted": true }))
}

/// GET /media/{id}.{ext}:按索引出 Content-Type;不存在(含 id/ext 组合错位)一律 404——
/// URL 即凭证,不区分「id 不存在」与「ext 不符」(不额外暴露信息)。
pub async fn handle_serve(
    State(s): State<std::sync::Arc<crate::server::AppState>>,
    headers: axum::http::HeaderMap,
    AxumPath(file): AxumPath<String>,
) -> Response {
    // 跨源页面无法带自定义 header 但会带 Referer;<img> 直接引用已知 uuid 会显示文件。
    // Referer 存在且非本机来源 → 拒(防跨源网页把本机媒体文件当图床);无 Referer(curl/直访)放行,
    // uuid 不可枚举已缓解。
    if let Some(referer) = headers
        .get(axum::http::header::REFERER)
        .and_then(|v| v.to_str().ok())
    {
        let ok = referer.starts_with("http://127.0.0.1:8787")
            || referer.starts_with("http://localhost:8787");
        if !ok {
            return (StatusCode::FORBIDDEN, "forbidden").into_response();
        }
    }
    let Some((id, ext)) = file.rsplit_once('.') else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let root = media_root(&s.codex_home);
    let mut idx = load_index(&root);
    let Some(item) = find_by_file(&idx, id, ext).cloned() else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let bytes = match std::fs::read(item_path(&root, &item)) {
        Ok(b) => b,
        Err(_) => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };
    // last_access 更新(读盘已发生,顺带回写索引;失败不影响服务)
    if let Some(i) = idx.items.iter_mut().find(|i| i.id == item.id) {
        i.last_access = chrono::Utc::now().timestamp();
    }
    let _ = save_index(&root, &idx);
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, item.mime.clone()),
            (header::CONTENT_LENGTH, bytes.len().to_string()),
            (header::CACHE_CONTROL, "private, max-age=3600".to_string()),
        ],
        bytes,
    )
        .into_response()
}

/// 标准 base64 解码(零依赖;接受带/不带 padding,拒绝非法字符与错误余数)。
pub(crate) fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    if s.len() % 4 == 1 {
        return Err("base64 长度非法".to_string());
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut nbits = 0u32;
    for (i, c) in s.bytes().enumerate() {
        if c == b'=' {
            break; // padding 之后的内容忽略(标准件 padding 只会在尾部)
        }
        let v = match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a' + 26) as u32,
            b'0'..=b'9' => (c - b'0' + 52) as u32,
            b'+' => 62,
            b'/' => 63,
            _ => return Err(format!("base64 非法字符(位置 {i})")),
        };
        acc = (acc << 6) | v;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "2xapi-media-ut-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 最小合法 PNG(1x1 红点)——真魔数+IHDR 结构,与探测资产同源。
    const RED_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    #[test]
    fn sniff_recognizes_whitelist_and_rejects_unknown() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n...").unwrap().mime, "image/png");
        assert_eq!(sniff(b"\xFF\xD8\xFF\xE0...").unwrap().mime, "image/jpeg");
        assert_eq!(sniff(b"GIF89a...").unwrap().mime, "image/gif");
        // RIFF 双签名分流
        let webp = b"RIFF\x00\x00\x00\x00WEBPVP8 ";
        assert_eq!(sniff(webp).unwrap().mime, "image/webp");
        let wav = b"RIFF\x00\x00\x00\x00WAVEfmt ";
        assert_eq!(sniff(wav).unwrap().mime, "audio/wav");
        assert_eq!(sniff(b"ID3\x04\x00...").unwrap().mime, "audio/mpeg");
        assert_eq!(sniff(&[0xFF, 0xFB, 0x90, 0x00]).unwrap().mime, "audio/mpeg");
        let mp4 = b"\x00\x00\x00\x20ftypisom";
        assert_eq!(sniff(mp4).unwrap().mime, "video/mp4");
        assert_eq!(sniff(b"\x1A\x45\xDF\xA3...").unwrap().mime, "video/webm");
        assert_eq!(sniff(b"%PDF-1.7...").unwrap().mime, "application/pdf");
        assert!(sniff(b"hello world not media").is_none());
        assert!(sniff(b"").is_none());
    }

    #[test]
    fn b64_decode_roundtrip_and_rejects_garbage() {
        assert_eq!(
            b64_decode(RED_PNG_B64).unwrap()[..4],
            [0x89, b'P', b'N', b'G']
        );
        assert_eq!(b64_decode("").unwrap(), Vec::<u8>::new());
        assert_eq!(b64_decode("QQ==").unwrap(), b"A".to_vec());
        assert_eq!(b64_decode("QUI=").unwrap(), b"AB".to_vec());
        assert_eq!(b64_decode("QUJD").unwrap(), b"ABC".to_vec());
        // 不带 padding 也接受
        assert_eq!(b64_decode("QQ").unwrap(), b"A".to_vec());
        assert!(b64_decode("A").is_err(), "长度 %4==1 非法");
        assert!(b64_decode("AB!C").is_err(), "非法字符拒绝");
    }

    #[test]
    fn upload_rejects_mime_mismatch() {
        let root = tmp_root("mismatch");
        let png = b64_decode(RED_PNG_B64).unwrap();
        // 声明 jpeg、实际 png → 拒
        let (code, _) = store_upload(&root, &png, "image/jpeg", "t").unwrap_err();
        assert_eq!(code, "E_MEDIA_MIME");
        // 未声明 → 以嗅探为准,通过
        let ok = store_upload(&root, &png, "", "t").unwrap();
        assert_eq!(ok.mime, "image/png");
    }

    #[test]
    fn quota_evicts_oldest_first_and_keeps_newest() {
        let root = tmp_root("lru");
        let png = b64_decode(RED_PNG_B64).unwrap();
        let a = store_upload(&root, &png, "", "t").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let b = store_upload(&root, &png, "", "t").unwrap();
        // 配额收紧到「只容一件」→ a(last_access 较旧)被逐出
        let mroot = media_root(&root);
        let mut idx = load_index(&mroot);
        idx.quota_bytes = png.len() as u64 + 1;
        evict_to_quota(&mroot, &mut idx);
        assert_eq!(idx.items.len(), 1, "应只剩一件");
        assert_eq!(idx.items[0].id, b.id, "逐出最旧的 a,保留较新的 b");
        assert!(!item_path(&mroot, &a).exists(), "a 的文件应被删除");
        assert!(item_path(&mroot, &b).exists());
    }

    #[test]
    fn index_roundtrip_and_ext_mismatch_is_hidden() {
        let root = tmp_root("idx");
        let mroot = media_root(&root);
        let item = store_upload(&root, &b64_decode(RED_PNG_B64).unwrap(), "", "ut").unwrap();
        let idx = load_index(&mroot);
        assert_eq!(idx.schema_version, 1);
        assert_eq!(idx.quota_bytes, DEFAULT_QUOTA_BYTES);
        assert_eq!(idx.items.len(), 1);
        assert_eq!(idx.items[0].id, item.id);
        // ext 错位必须查不到(404 语义)
        assert!(find_by_file(&idx, &item.id, "png").is_some());
        assert!(find_by_file(&idx, &item.id, "jpg").is_none());
        assert!(find_by_file(&idx, "no-such-id", "png").is_none());
    }

    /// 真机 e2e(#[ignore],手动驱动):真实 ~/.codex/media 首建目录,上传→取回字节一致→删除→零残留。
    /// 只动 {codex_home}/media(全新目录),不碰任何既有配置。
    #[test]
    #[ignore]
    fn media_real_machine_e2e() {
        let codex =
            std::path::PathBuf::from(std::env::var("HOME").expect("需要 HOME")).join(".codex");
        let mroot = media_root(&codex);
        let existed_before = mroot.exists();
        let png = b64_decode(RED_PNG_B64).unwrap();
        let item = store_upload(&codex, &png, "image/png", "real-e2e").expect("上传");
        let got = std::fs::read(item_path(&mroot, &item)).expect("取回");
        assert_eq!(got, png, "字节一致");
        // 删除还原:条目+文件;本批新建且目录已空 → 整目录移除
        let mut idx = load_index(&mroot);
        idx.items.retain(|i| i.id != item.id);
        let _ = std::fs::remove_file(item_path(&mroot, &item));
        save_index(&mroot, &idx).unwrap();
        if !existed_before {
            // 本批新建且目录内仅剩空索引 → 连 index.json 一起清,零残留
            let only_index = mroot
                .read_dir()
                .map(|d| {
                    let names: Vec<String> = d
                        .filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect();
                    names.len() == 1 && names[0] == "index.json"
                })
                .unwrap_or(false);
            if only_index {
                let _ = std::fs::remove_file(mroot.join("index.json"));
                let _ = std::fs::remove_dir(&mroot);
            }
        }
    }
}
