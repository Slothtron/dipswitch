//! 抓包日志：把 HAR 与 Charles 会话解析成统一模型。
//!
//! 支持四种文件，按内容嗅探而不是只认扩展名：
//!
//! - 浏览器 / DevTools 导出的 **HAR 1.2**（JSON，根对象里有 `log.entries`）；
//! - Charles 的 **JSON Session**（`.chlsj`，根是一个数组）；
//! - Charles 的 **XML Session**（`.chls`，根是 `charles-session`）；
//! - Charles 的 **二进制 Session**（`.chls`，Java 对象序列化流，见 `chls` 子模块）。
//!
//! 这里只做「文件 → 模型」，不碰 GPUI，也不做排序和过滤。`Entry::path` 只含路径，
//! 查询串单独放在 `Entry::query`，由界面决定在列表里省略、在详情里展开。正文只保留
//! 预览：单个正文最多 [`BODY_PREVIEW_BYTES`]，整份日志合计最多
//! [`TOTAL_PREVIEW_BYTES`]，超出部分用 `truncated` / `binary` 如实标记，绝不把完整
//! 报文静默丢给内存。压缩正文在二进制会话里先按 `Content-Encoding` 解压再取预览。

use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use serde::Deserialize;

/// 单个文件上限：超过就拒绝，避免一次误选把整个窗口拖死。
pub const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// 单次导入最多保留的请求条数；超出的部分不会进入列表。
pub const MAX_ENTRIES: usize = 50_000;
/// 单个请求 / 响应正文保留的预览字节数。
pub const BODY_PREVIEW_BYTES: usize = 64 * 1024;
/// 整份日志所有正文预览的合计预算。
pub const TOTAL_PREVIEW_BYTES: usize = 32 * 1024 * 1024;

mod chls;

/// 日志格式。UI 只用来显示来源，不参与后续判断。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureFormat {
    Har,
    CharlesJson,
    CharlesXml,
    /// Charles 的二进制 Session File（Java 对象序列化）。
    CharlesBinary,
}

impl CaptureFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::Har => "HAR",
            Self::CharlesJson => "Charles JSON",
            Self::CharlesXml => "Charles XML",
            Self::CharlesBinary => "Charles binary",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub name: String,
    pub value: String,
}

/// 正文的**预览**，不是完整正文。`size` 是完整大小（未知时为 `None`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Body {
    /// 文本预览；`None` 表示正文是二进制、未捕获或超出预览预算。
    pub preview: Option<String>,
    /// 完整正文的字节数（编码后的大小按解码后估算）。
    pub size: Option<u64>,
    /// 预览比完整正文短。
    pub truncated: bool,
    /// 内容不是合法 UTF-8（按二进制展示，不做猜测解码）。
    pub binary: bool,
    /// 二进制正文的前若干字节。文本正文不给这个字段——Hex 视图直接用预览的字节，
    /// 只有「没有文本可依」的二进制才需要把原始字节留下来。
    pub bytes: Option<Vec<u8>>,
    pub mime: String,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub method: String,
    pub url: String,
    pub host: String,
    /// 只含路径，不含查询串与片段：列表按路径对齐，参数在详情里看。
    pub path: String,
    /// 原始查询串（不含 `?`）。
    pub query: Option<String>,
    pub status: Option<u16>,
    /// 失败原因或非 HTTP 结果的说明。
    pub note: Option<String>,
    /// HTTP 版本（`http/1.1`、`h2`…）。只有 HAR 记录了这个字段，其余格式为 `None`。
    pub protocol: Option<String>,
    pub mime: String,
    pub size: Option<u64>,
    pub duration_ms: Option<f64>,
    /// 原始 ISO-8601 起始时间，展示层只截取时间部分。
    pub started: Option<String>,
    pub remote_address: Option<String>,
    pub request_headers: Vec<Header>,
    pub response_headers: Vec<Header>,
    pub request_body: Option<Body>,
    pub response_body: Option<Body>,
}

#[derive(Debug, Clone)]
pub struct CaptureLog {
    pub format: CaptureFormat,
    pub file_name: String,
    pub entries: Vec<Entry>,
    /// 文件里的请求数超过 [`MAX_ENTRIES`]，列表只保留了前面一部分。
    pub truncated: bool,
    pub warnings: Vec<String>,
}

/// HTTP 状态码的后果分类，供界面选颜色；不做业务判断。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusClass {
    Success,
    Redirect,
    ClientError,
    ServerError,
    Pending,
}

pub fn status_class(status: Option<u16>) -> StatusClass {
    match status {
        Some(200..=299) => StatusClass::Success,
        Some(300..=399) => StatusClass::Redirect,
        Some(400..=499) => StatusClass::ClientError,
        Some(500..=599) => StatusClass::ServerError,
        _ => StatusClass::Pending,
    }
}

/// HTTP 版本写成一个短的徽标文本：`HTTP/1.1` → `http/1.1`，`h2` → `h2`。
///
/// 只做归一，不做判断：会话里写什么版本就显示什么，认不出的串原样保留。
fn protocol_label(version: &str) -> Option<String> {
    let trimmed = version.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lowered = trimmed.to_ascii_lowercase();
    Some(match lowered.as_str() {
        "h2" => "h2".to_string(),
        "h2c" => "h2c".to_string(),
        "h3" => "h3".to_string(),
        "http/2" | "http/2.0" => "h2".to_string(),
        "http/1.1" => "http/1.1".to_string(),
        "http/1.0" => "http/1.0".to_string(),
        _ => trimmed.to_string(),
    })
}

/// 读取并解析一个抓包文件。文件超过上限时直接拒绝，不做截断读取。
pub fn load(path: &Path) -> Result<CaptureLog, String> {
    let metadata =
        std::fs::metadata(path).map_err(|error| format!("Could not read the file: {error}"))?;
    if metadata.len() > MAX_FILE_BYTES {
        return Err(format!(
            "File is {:.1} MB, over the {} MB limit",
            metadata.len() as f64 / (1024.0 * 1024.0),
            MAX_FILE_BYTES / (1024 * 1024)
        ));
    }
    let bytes = std::fs::read(path).map_err(|error| format!("Could not read the file: {error}"))?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "capture".to_string());
    parse_bytes(&name, &bytes)
}

fn parse_bytes(file_name: &str, bytes: &[u8]) -> Result<CaptureLog, String> {
    if bytes.starts_with(&chls::MAGIC) {
        return chls::parse(file_name, bytes);
    }
    if bytes.starts_with(b"PK") {
        return Err(
            "This looks like a Charles native archive (ZIP). Export it as JSON Session (.chlsj) or HAR and import that instead."
                .to_string(),
        );
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| "File is not UTF-8 text; cannot parse a HAR or Charles session".to_string())?;
    // BOM 只在嗅探时被忽略的话，JSON 解析仍会失败。
    let text = text.trim_start_matches('\u{feff}');
    let head = text.trim_start_matches([' ', '\t', '\r', '\n']);
    match head.chars().next() {
        Some('<') => parse_charles_xml(file_name, text),
        Some('{') | Some('[') => parse_json(file_name, text),
        _ => Err(
            "Unrecognized format: expected HAR, Charles JSON Session (.chlsj) or Charles XML Session (.chls)"
                .to_string(),
        ),
    }
}

fn parse_json(file_name: &str, text: &str) -> Result<CaptureLog, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|error| format!("JSON parse failed: {error}"))?;
    match &value {
        serde_json::Value::Object(map) if map.contains_key("log") => {
            let file: HarFile =
                serde_json::from_value(value).map_err(|error| format!("HAR structure is invalid: {error}"))?;
            Ok(parse_har(file_name, file))
        }
        serde_json::Value::Array(_) => {
            let transactions: Vec<CharlesTxn> = serde_json::from_value(value)
                .map_err(|error| format!("Charles JSON structure is invalid: {error}"))?;
            Ok(parse_charles_json(file_name, transactions))
        }
        _ => {
            Err("JSON is neither a HAR (no log.entries) nor a Charles session (root should be an array)".to_string())
        }
    }
}

// ------------------------------------------------------------------ 预算

/// 全份日志共享的正文预览预算。
struct BodyBudget {
    remaining: usize,
}

impl BodyBudget {
    fn new() -> Self {
        Self {
            remaining: TOTAL_PREVIEW_BYTES,
        }
    }

    fn take(&mut self, wanted: usize) -> usize {
        let allowed = wanted.min(BODY_PREVIEW_BYTES).min(self.remaining);
        self.remaining -= allowed;
        allowed
    }
}

fn mime_from(headers: &[Header], fallback: &str) -> String {
    headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("content-type"))
        .map(|header| header.value.clone())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

/// 文本正文：保留前 `allowed` 字节，截断按字符边界。
fn text_body(text: &str, mime: &str, budget: &mut BodyBudget) -> Body {
    if text.is_empty() {
        return Body {
            mime: mime.to_string(),
            ..Body::default()
        };
    }
    let allowed = budget.take(text.len());
    let preview = truncate_chars(text, allowed);
    Body {
        size: Some(text.len() as u64),
        truncated: allowed < text.len(),
        preview: Some(preview),
        binary: false,
        bytes: None,
        mime: mime.to_string(),
    }
}

/// 编码过的正文（HAR 的 `encoding: base64`、Charles 的 `encoded`）。
fn encoded_body(
    encoded: &str,
    mime: &str,
    declared_size: Option<u64>,
    budget: &mut BodyBudget,
) -> Body {
    let size = declared_size.unwrap_or_else(|| base64_decoded_len(encoded) as u64);
    let cap = BODY_PREVIEW_BYTES.min(budget.remaining);
    let Some(mut bytes) = decode_base64_prefix(encoded, cap) else {
        return Body {
            size: Some(size),
            truncated: true,
            binary: true,
            bytes: None,
            mime: mime.to_string(),
            preview: None,
        };
    };
    let allowed = budget.take(bytes.len());
    bytes.truncate(allowed);
    let truncated = (size as usize) > bytes.len();
    let text = std::str::from_utf8(&bytes)
        .ok()
        .map(|text| text.to_string());
    match text {
        Some(text) => Body {
            preview: Some(text),
            size: Some(size),
            truncated,
            binary: false,
            bytes: None,
            mime: mime.to_string(),
        },
        None => Body {
            preview: None,
            size: Some(size),
            truncated,
            binary: true,
            bytes: Some(bytes),
            mime: mime.to_string(),
        },
    }
}

fn truncate_chars(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// base64 解码前若干字节；只解码预览需要的部分，避免为一次展示解出整段报文。
fn decode_base64_prefix(encoded: &str, max_bytes: usize) -> Option<Vec<u8>> {
    let compact: String = encoded
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect();
    if compact.is_empty() {
        return Some(Vec::new());
    }
    let needed_chars = max_bytes.div_ceil(3) * 4;
    let take = compact.len().min(needed_chars);
    let take = take - take % 4;
    let slice = &compact[..take];
    BASE64.decode(slice.as_bytes()).ok()
}

/// base64 解码后的字节数，不解码内容。
fn base64_decoded_len(encoded: &str) -> usize {
    let mut len = 0usize;
    let mut padding = 0usize;
    for byte in encoded.bytes() {
        if byte.is_ascii_whitespace() {
            continue;
        }
        len += 1;
        if byte == b'=' {
            padding += 1;
        }
    }
    (len / 4) * 3 - padding.min(2)
}

/// 把 URL 拆成 host、路径、查询串。路径不带 `?query` 与 `#fragment`。
fn split_url(url: &str) -> (String, String, Option<String>) {
    let Some((_, rest)) = url.split_once("://") else {
        let (path, query) = strip_query(url);
        return (String::new(), path, query);
    };
    let rest = rest.split_once('#').map_or(rest, |(head, _)| head);
    let (host, tail) = match rest.split_once('/') {
        Some((host, tail)) => (host, format!("/{tail}")),
        None => (rest, "/".to_string()),
    };
    let (path, query) = strip_query(&tail);
    (host.to_string(), path, query)
}

/// `"/a/b?x=1#top"` → `("/a/b", Some("x=1"))`。
pub(crate) fn strip_query(target: &str) -> (String, Option<String>) {
    let target = target.split_once('#').map_or(target, |(head, _)| head);
    match target.split_once('?') {
        Some((path, query)) => (path.to_string(), non_empty(query)),
        None => (target.to_string(), None),
    }
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

/// 解析查询串或表单正文；键值都做百分号解码，`+` 还原为空格。
pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => (percent_decode(key), percent_decode(value)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                match (hex_value(bytes[index + 1]), hex_value(bytes[index + 2])) {
                    (Some(high), Some(low)) => {
                        out.push(high * 16 + low);
                        index += 3;
                    }
                    _ => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn squash(
    mut entries: Vec<Entry>,
    total: usize,
    mut warnings: Vec<String>,
) -> (Vec<Entry>, bool, Vec<String>) {
    let truncated = total > MAX_ENTRIES;
    if truncated {
        warnings.push(format!(
            "File holds {total} requests; only the first {MAX_ENTRIES} are loaded"
        ));
        entries.truncate(MAX_ENTRIES);
    }
    (entries, truncated, warnings)
}

// ------------------------------------------------------------------ HAR

#[derive(Debug, Deserialize)]
struct HarFile {
    log: HarLog,
}

#[derive(Debug, Deserialize)]
struct HarLog {
    #[serde(default)]
    entries: Vec<HarEntry>,
}

#[derive(Debug, Default, Deserialize)]
struct HarEntry {
    #[serde(default, rename = "startedDateTime")]
    started: Option<String>,
    #[serde(default)]
    time: Option<f64>,
    #[serde(default)]
    request: Option<HarRequest>,
    #[serde(default)]
    response: Option<HarResponse>,
    #[serde(default, rename = "serverIPAddress")]
    server_ip: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct HarRequest {
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default, rename = "httpVersion")]
    http_version: Option<String>,
    #[serde(default)]
    headers: Vec<NameValue>,
    #[serde(default, rename = "postData")]
    post_data: Option<HarPostData>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct NameValue {
    #[serde(default)]
    name: String,
    #[serde(default)]
    value: String,
}

#[derive(Debug, Default, Deserialize)]
struct HarPostData {
    #[serde(default, rename = "mimeType")]
    mime: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    params: Vec<HarParam>,
}

#[derive(Debug, Default, Deserialize)]
struct HarParam {
    #[serde(default)]
    name: String,
    #[serde(default)]
    value: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct HarResponse {
    #[serde(default)]
    status: Option<i64>,
    #[serde(default, rename = "httpVersion")]
    http_version: Option<String>,
    #[serde(default)]
    headers: Vec<NameValue>,
    #[serde(default)]
    content: Option<HarContent>,
    #[serde(default, rename = "bodySize")]
    body_size: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
struct HarContent {
    #[serde(default)]
    size: Option<i64>,
    #[serde(default, rename = "mimeType")]
    mime: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    encoding: Option<String>,
}

fn parse_har(file_name: &str, file: HarFile) -> CaptureLog {
    let total = file.log.entries.len();
    let mut budget = BodyBudget::new();
    let mut warnings = Vec::new();
    let mut entries = Vec::with_capacity(total.min(MAX_ENTRIES));
    for (index, har) in file.log.entries.iter().enumerate() {
        if index >= MAX_ENTRIES {
            break;
        }
        entries.push(har_entry(har, &mut budget, &mut warnings, index));
    }
    let (entries, truncated, warnings) = squash(entries, total, warnings);
    CaptureLog {
        format: CaptureFormat::Har,
        file_name: file_name.to_string(),
        entries,
        truncated,
        warnings,
    }
}

fn har_entry(
    har: &HarEntry,
    budget: &mut BodyBudget,
    warnings: &mut Vec<String>,
    index: usize,
) -> Entry {
    let request = har.request.as_ref();
    let response = har.response.as_ref();
    let method = request
        .and_then(|request| request.method.clone())
        .filter(|method| !method.is_empty())
        .unwrap_or_else(|| "GET".to_string());
    let url = request
        .and_then(|request| request.url.clone())
        .unwrap_or_default();
    let (host, path, query) = split_url(&url);
    let request_headers: Vec<Header> = request
        .map(|request| to_headers(&request.headers))
        .unwrap_or_default();
    let response_headers: Vec<Header> = response
        .map(|response| to_headers(&response.headers))
        .unwrap_or_default();
    let content = response.and_then(|response| response.content.as_ref());
    let mime = content
        .and_then(|content| content.mime.clone())
        .filter(|mime| !mime.is_empty())
        .unwrap_or_else(|| mime_from(&response_headers, ""));
    let status = response
        .and_then(|response| response.status)
        .and_then(|status| u16::try_from(status).ok())
        .filter(|status| *status > 0);
    if response.is_none() {
        warnings.push(format!(
            "Entry {} has no response; shown as incomplete",
            index + 1
        ));
    }
    let response_body = content.and_then(|content| har_body(content, &mime, response, budget));
    let size = response_body
        .as_ref()
        .and_then(|body| body.size)
        .or_else(|| {
            response
                .and_then(|response| response.body_size)
                .filter(|size| *size >= 0)
                .map(|size| size as u64)
        });
    let request_body = request
        .and_then(|request| request.post_data.as_ref())
        .map(|post| har_request_body(post, budget))
        .filter(|body| body.preview.is_some() || body.size.unwrap_or(0) > 0);

    Entry {
        method,
        host,
        path,
        query,
        url,
        status,
        note: None,
        protocol: request
            .and_then(|request| request.http_version.as_deref())
            .or_else(|| response.and_then(|response| response.http_version.as_deref()))
            .and_then(protocol_label),
        mime,
        size,
        duration_ms: har.time.filter(|time| *time >= 0.0),
        started: har.started.clone(),
        remote_address: har.server_ip.clone(),
        request_headers,
        response_headers,
        request_body,
        response_body,
    }
}

fn har_body(
    content: &HarContent,
    mime: &str,
    response: Option<&HarResponse>,
    budget: &mut BodyBudget,
) -> Option<Body> {
    let declared = content
        .size
        .filter(|size| *size >= 0)
        .map(|size| size as u64)
        .or_else(|| {
            response
                .and_then(|response| response.body_size)
                .filter(|size| *size >= 0)
                .map(|size| size as u64)
        });
    match (&content.text, content.encoding.as_deref()) {
        (Some(text), Some("base64")) => Some(encoded_body(text, mime, declared, budget)),
        (Some(text), _) => Some(text_body(text, mime, budget)),
        (None, _) => declared.filter(|size| *size > 0).map(|size| Body {
            preview: None,
            size: Some(size),
            truncated: false,
            binary: false,
            bytes: None,
            mime: mime.to_string(),
        }),
    }
}

fn har_request_body(post: &HarPostData, budget: &mut BodyBudget) -> Body {
    let mime = post.mime.clone().unwrap_or_default();
    if let Some(text) = post.text.as_deref() {
        return text_body(text, &mime, budget);
    }
    if post.params.is_empty() {
        return Body {
            mime,
            ..Body::default()
        };
    }
    let joined = post
        .params
        .iter()
        .map(|param| match &param.value {
            Some(value) => format!("{}={}", param.name, value),
            None => param.name.clone(),
        })
        .collect::<Vec<_>>()
        .join("&");
    text_body(&joined, &mime, budget)
}

fn to_headers(headers: &[NameValue]) -> Vec<Header> {
    headers
        .iter()
        .map(|header| Header {
            name: header.name.clone(),
            value: header.value.clone(),
        })
        .collect()
}

// ------------------------------------------------------------------ Charles JSON

#[derive(Debug, Default, Deserialize)]
struct CharlesTxn {
    #[serde(default, alias = "protocol")]
    scheme: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    port: Option<i64>,
    #[serde(default, rename = "actualPort")]
    actual_port: Option<i64>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default, rename = "errorMessage")]
    error_message: Option<String>,
    #[serde(default)]
    request: Option<CharlesMessage>,
    #[serde(default)]
    response: Option<CharlesMessage>,
    #[serde(default)]
    times: Option<serde_json::Value>,
    #[serde(default)]
    durations: Option<serde_json::Value>,
    #[serde(default, rename = "startTime")]
    start_time: Option<String>,
    #[serde(default, rename = "remoteAddress")]
    remote_address: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct CharlesMessage {
    #[serde(default)]
    status: Option<i64>,
    #[serde(default)]
    header: Option<CharlesHeader>,
    #[serde(default)]
    body: Option<CharlesBody>,
    #[serde(default)]
    sizes: Option<CharlesSizes>,
    #[serde(default, rename = "mimeType")]
    mime: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct CharlesHeader {
    #[serde(default)]
    headers: Vec<NameValue>,
}

#[derive(Debug, Default, Deserialize)]
struct CharlesBody {
    #[serde(default)]
    encoded: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default, rename = "mimeType")]
    mime: Option<String>,
    #[serde(default)]
    size: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
struct CharlesSizes {
    #[serde(default)]
    body: Option<i64>,
}

fn parse_charles_json(file_name: &str, transactions: Vec<CharlesTxn>) -> CaptureLog {
    let total = transactions.len();
    let mut budget = BodyBudget::new();
    let mut warnings = Vec::new();
    let mut entries = Vec::with_capacity(total.min(MAX_ENTRIES));
    for (index, txn) in transactions.into_iter().enumerate() {
        if index >= MAX_ENTRIES {
            break;
        }
        entries.push(charles_json_entry(txn, &mut budget, &mut warnings, index));
    }
    let (entries, truncated, warnings) = squash(entries, total, warnings);
    CaptureLog {
        format: CaptureFormat::CharlesJson,
        file_name: file_name.to_string(),
        entries,
        truncated,
        warnings,
    }
}

fn charles_json_entry(
    txn: CharlesTxn,
    budget: &mut BodyBudget,
    warnings: &mut Vec<String>,
    index: usize,
) -> Entry {
    let scheme = txn
        .scheme
        .clone()
        .filter(|scheme| !scheme.is_empty())
        .unwrap_or_else(|| "http".to_string());
    let host = txn.host.clone().unwrap_or_default();
    let port = txn.actual_port.or(txn.port);
    let mut url = format!("{scheme}://{host}");
    if let Some(port) = port.filter(|port| !is_default_port(&scheme, *port)) {
        url.push(':');
        url.push_str(&port.to_string());
    }
    let path = txn.path.clone().unwrap_or_else(|| "/".to_string());
    url.push_str(&path);
    let query = txn.query.as_deref().and_then(non_empty);
    if let Some(query) = query.as_deref() {
        url.push('?');
        url.push_str(query);
    }

    let request = txn.request.as_ref();
    let response = txn.response.as_ref();
    let request_headers = request
        .and_then(|request| request.header.as_ref())
        .map(|header| to_headers(&header.headers))
        .unwrap_or_default();
    let response_headers = response
        .and_then(|response| response.header.as_ref())
        .map(|header| to_headers(&header.headers))
        .unwrap_or_default();
    let mime = response
        .and_then(|response| response.mime.clone())
        .or_else(|| {
            response
                .and_then(|response| response.body.as_ref())
                .and_then(|body| body.mime.clone())
        })
        .filter(|mime| !mime.is_empty())
        .unwrap_or_else(|| mime_from(&response_headers, ""));
    let response_body = response.and_then(|response| charles_body(response, &mime, budget));
    let request_body =
        request.and_then(|request| charles_body(request, &mime_from(&request_headers, ""), budget));
    let status = response
        .and_then(|response| response.status)
        .and_then(|status| u16::try_from(status).ok());
    let note = txn.error_message.clone().or_else(|| {
        txn.status
            .as_deref()
            .filter(|status| is_failure_state(status))
            .map(|status| format!("Charles: {status}"))
    });
    if request.is_none() || response.is_none() {
        warnings.push(format!(
            "Entry {} is missing its request or response",
            index + 1
        ));
    }
    Entry {
        method: txn
            .method
            .filter(|method| !method.is_empty())
            .unwrap_or_else(|| "GET".to_string()),
        host,
        path,
        query,
        url,
        status: if note.is_some() { None } else { status },
        note,
        protocol: None,
        mime,
        size: response
            .and_then(|response| response.sizes.as_ref().and_then(|sizes| sizes.body))
            .filter(|size| *size >= 0)
            .map(|size| size as u64)
            .or_else(|| response_body.as_ref().and_then(|body| body.size)),
        duration_ms: txn
            .durations
            .as_ref()
            .and_then(|durations| number(durations, "total")),
        started: txn
            .start_time
            .clone()
            .or_else(|| txn.times.as_ref().and_then(|times| string(times, "start"))),
        remote_address: txn.remote_address,
        request_headers,
        response_headers,
        request_body,
        response_body,
    }
}

fn charles_body(
    message: &CharlesMessage,
    fallback_mime: &str,
    budget: &mut BodyBudget,
) -> Option<Body> {
    let body = message.body.as_ref()?;
    let mime = body
        .mime
        .clone()
        .or_else(|| message.mime.clone())
        .filter(|mime| !mime.is_empty())
        .unwrap_or_else(|| fallback_mime.to_string());
    let declared = body
        .size
        .filter(|size| *size >= 0)
        .map(|size| size as u64)
        .or_else(|| {
            message
                .sizes
                .as_ref()
                .and_then(|sizes| sizes.body)
                .filter(|size| *size >= 0)
                .map(|size| size as u64)
        });
    match (body.encoded.as_deref(), body.text.as_deref()) {
        (Some(encoded), _) if !encoded.is_empty() => {
            Some(encoded_body(encoded, &mime, declared, budget))
        }
        (_, Some(text)) => Some(text_body(text, &mime, budget)),
        _ => declared.filter(|size| *size > 0).map(|size| Body {
            preview: None,
            size: Some(size),
            truncated: false,
            binary: false,
            bytes: None,
            mime,
        }),
    }
}

fn is_default_port(scheme: &str, port: i64) -> bool {
    (scheme == "http" && port == 80) || (scheme == "https" && port == 443)
}

fn is_failure_state(status: &str) -> bool {
    let status = status.to_ascii_uppercase();
    ["EXCEPTION", "FAIL", "ABORT", "ERROR", "TIMEOUT"]
        .iter()
        .any(|marker| status.contains(marker))
}

fn number(value: &serde_json::Value, key: &str) -> Option<f64> {
    value.get(key).and_then(|value| match value {
        serde_json::Value::Number(number) => number.as_f64(),
        serde_json::Value::String(text) => text.parse().ok(),
        _ => None,
    })
}

fn string(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

// ------------------------------------------------------------------ Charles XML

/// XML 里 `<body>` 的累积器：只留预览，但记住完整长度与 padding，用来估算大小。
#[derive(Default)]
struct XmlBody {
    preview: String,
    total: usize,
    encoded_chars: usize,
    padding: usize,
    encoding: Option<String>,
    mime: String,
    declared_size: Option<u64>,
    cap: usize,
}

impl XmlBody {
    fn push(&mut self, text: &str) {
        self.total += text.len();
        self.encoded_chars += text
            .bytes()
            .filter(|byte| !byte.is_ascii_whitespace())
            .count();
        self.padding += text.bytes().filter(|byte| *byte == b'=').count();
        if self.preview.len() < self.cap {
            let room = self.cap - self.preview.len();
            self.preview.push_str(&truncate_chars(text, room));
        }
    }
}

#[derive(Default)]
struct XmlTxn {
    method: Option<String>,
    scheme: Option<String>,
    host: Option<String>,
    port: Option<i64>,
    path: Option<String>,
    query: Option<String>,
    start_time: Option<String>,
    start_ms: Option<i64>,
    response_ms: Option<i64>,
    end_ms: Option<i64>,
    remote_address: Option<String>,
    state: Option<String>,
    request_headers: Vec<Header>,
    response_headers: Vec<Header>,
    response_status: Option<u16>,
    response_mime: Option<String>,
    response_size: Option<u64>,
    request_body: Option<XmlBody>,
    response_body: Option<XmlBody>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum XmlContext {
    Request,
    Response,
}

enum Grab {
    HeaderName,
    HeaderValue,
    FirstLine,
    Body,
    Ignored,
}

fn parse_charles_xml(file_name: &str, text: &str) -> Result<CaptureLog, String> {
    let text = text.trim_start_matches('\u{feff}');
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(false);
    reader.config_mut().expand_empty_elements = true;
    reader.config_mut().check_end_names = false;

    let mut total = 0usize;
    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    let mut budget = BodyBudget::new();
    let mut txn: Option<XmlTxn> = None;
    let mut context: Option<XmlContext> = None;
    let mut grab: Option<Grab> = None;
    let mut buffer = String::new();
    let mut header_name = String::new();
    let mut first_line: Option<String> = None;
    let mut saw_root = false;

    loop {
        let event = reader
            .read_event()
            .map_err(|error| format!("XML parse failed: {error}"))?;
        match event {
            Event::Start(start) => {
                let name = local_name(&start);
                if name == "charles-session" {
                    saw_root = true;
                    continue;
                }
                if name == "transaction" {
                    txn = Some(xml_transaction(&start));
                    context = None;
                    grab = None;
                    continue;
                }
                let Some(current) = txn.as_mut() else {
                    continue;
                };
                match name.as_str() {
                    "request" => context = Some(XmlContext::Request),
                    "response" => {
                        context = Some(XmlContext::Response);
                        current.response_status = attr_u16(&start, "status");
                        current.response_mime = attr(&start, "mime-type");
                        current.response_size = attr_u64(&start, "bodySize");
                    }
                    "header" => {
                        header_name.clear();
                        buffer.clear();
                    }
                    "name" => {
                        buffer.clear();
                        grab = Some(Grab::HeaderName);
                    }
                    "value" => {
                        buffer.clear();
                        grab = Some(Grab::HeaderValue);
                    }
                    "first-line" => {
                        buffer.clear();
                        grab = Some(Grab::FirstLine);
                    }
                    "body" => {
                        let body = XmlBody {
                            encoding: attr(&start, "encoding"),
                            mime: attr(&start, "mime-type").unwrap_or_default(),
                            declared_size: attr_u64(&start, "bodySize"),
                            cap: BODY_PREVIEW_BYTES * 2,
                            ..XmlBody::default()
                        };
                        match context {
                            Some(XmlContext::Request) => current.request_body = Some(body),
                            Some(XmlContext::Response) => current.response_body = Some(body),
                            None => {}
                        }
                        grab = Some(Grab::Body);
                    }
                    _ => grab = Some(Grab::Ignored),
                }
            }
            Event::Text(text) => {
                let content = text.xml_content(quick_xml::XmlVersion::Explicit1_0);
                append(&grab, &mut buffer, &mut txn, context, &content);
            }
            Event::CData(text) => {
                let content = text.xml_content(quick_xml::XmlVersion::Explicit1_0);
                append(&grab, &mut buffer, &mut txn, context, &content);
            }
            Event::GeneralRef(reference) => {
                let resolved = resolve_reference(&reference);
                append(&grab, &mut buffer, &mut txn, context, &resolved);
            }
            Event::End(end) => {
                let name = local_name_end(&end);
                if name == "transaction" {
                    total += 1;
                    if entries.len() < MAX_ENTRIES
                        && let Some(finished) = txn.take()
                    {
                        entries.push(xml_entry(finished, &mut budget, &mut warnings, total));
                    }
                    context = None;
                    grab = None;
                    first_line = None;
                    buffer.clear();
                    continue;
                }
                let Some(current) = txn.as_mut() else {
                    continue;
                };
                match name.as_str() {
                    "name" => {
                        header_name = std::mem::take(&mut buffer);
                        grab = None;
                    }
                    "value" => {
                        if !header_name.is_empty() {
                            let header = Header {
                                name: std::mem::take(&mut header_name),
                                value: std::mem::take(&mut buffer),
                            };
                            match context {
                                Some(XmlContext::Request) => current.request_headers.push(header),
                                Some(XmlContext::Response) => current.response_headers.push(header),
                                None => {}
                            }
                        } else {
                            buffer.clear();
                        }
                        grab = None;
                    }
                    "first-line" => {
                        first_line = Some(std::mem::take(&mut buffer));
                        grab = None;
                    }
                    "request" => {
                        context = None;
                        grab = None;
                    }
                    "response" => {
                        if current.response_status.is_none() {
                            current.response_status =
                                first_line.as_deref().and_then(status_from_first_line);
                        }
                        context = None;
                        grab = None;
                        first_line = None;
                    }
                    _ => {
                        if matches!(grab, Some(Grab::Body)) {
                            grab = None;
                        }
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    if !saw_root {
        return Err("Not a Charles XML session: missing the charles-session root".to_string());
    }
    let (entries, truncated, warnings) = squash(entries, total, warnings);
    Ok(CaptureLog {
        format: CaptureFormat::CharlesXml,
        file_name: file_name.to_string(),
        entries,
        truncated,
        warnings,
    })
}

fn append(
    grab: &Option<Grab>,
    buffer: &mut String,
    txn: &mut Option<XmlTxn>,
    context: Option<XmlContext>,
    text: &str,
) {
    match grab {
        Some(Grab::Body) => {
            let (Some(current), Some(context)) = (txn.as_mut(), context) else {
                return;
            };
            let body = match context {
                XmlContext::Request => current.request_body.as_mut(),
                XmlContext::Response => current.response_body.as_mut(),
            };
            if let Some(body) = body {
                body.push(text);
            }
        }
        Some(Grab::Ignored) | None => {}
        Some(_) => {
            if buffer.len() < 8 * 1024 {
                buffer.push_str(text);
            }
        }
    }
}

fn xml_transaction(start: &BytesStart<'_>) -> XmlTxn {
    XmlTxn {
        method: attr(start, "method"),
        scheme: attr(start, "protocol"),
        host: attr(start, "host"),
        port: attr(start, "port").and_then(|port| port.parse().ok()),
        path: attr(start, "path"),
        query: attr(start, "query"),
        start_time: attr(start, "startTime"),
        start_ms: attr_i64(start, "startTimeMillis"),
        response_ms: attr_i64(start, "responseTimeMillis"),
        end_ms: attr_i64(start, "endTimeMillis"),
        remote_address: attr(start, "remoteAddress"),
        state: attr(start, "status"),
        response_status: attr_u16(start, "responseStatus"),
        response_mime: attr(start, "responseMimeType"),
        response_size: attr_u64(start, "responseBodySize"),
        ..XmlTxn::default()
    }
}

fn xml_entry(
    txn: XmlTxn,
    budget: &mut BodyBudget,
    warnings: &mut Vec<String>,
    index: usize,
) -> Entry {
    let scheme = txn
        .scheme
        .filter(|scheme| !scheme.is_empty())
        .unwrap_or_else(|| "http".to_string());
    let host = txn.host.unwrap_or_default();
    let path = txn
        .path
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| "/".to_string());
    let mut url = format!("{scheme}://{host}");
    if let Some(port) = txn.port.filter(|port| !is_default_port(&scheme, *port)) {
        url.push(':');
        url.push_str(&port.to_string());
    }
    url.push_str(&path);
    let query = txn.query.filter(|query| !query.is_empty());
    if let Some(query) = query.as_deref() {
        url.push('?');
        url.push_str(query);
    }
    let mime = txn
        .response_mime
        .clone()
        .filter(|mime| !mime.is_empty())
        .unwrap_or_else(|| mime_from(&txn.response_headers, ""));
    let response_body = xml_body(txn.response_body, &mime, budget);
    let request_body = xml_body(
        txn.request_body,
        &mime_from(&txn.request_headers, ""),
        budget,
    );
    let note = txn
        .state
        .as_deref()
        .filter(|state| is_failure_state(state) || *state != "COMPLETE")
        .map(|state| format!("Charles: {state}"));
    if note.is_some() {
        warnings.push(format!(
            "Entry {index} is not complete: {}",
            txn.state.as_deref().unwrap_or("UNKNOWN")
        ));
    }
    let duration = match (txn.start_ms, txn.end_ms.or(txn.response_ms)) {
        (Some(start), Some(end)) if end >= start => Some((end - start) as f64),
        _ => None,
    };
    Entry {
        method: txn
            .method
            .filter(|method| !method.is_empty())
            .unwrap_or_else(|| "GET".to_string()),
        host,
        path,
        query,
        url,
        status: if note.is_some() {
            None
        } else {
            txn.response_status
        },
        note,
        protocol: None,
        mime,
        size: response_body
            .as_ref()
            .and_then(|body| body.size)
            .or(txn.response_size),
        duration_ms: duration,
        started: txn.start_time,
        remote_address: txn.remote_address,
        request_headers: txn.request_headers,
        response_headers: txn.response_headers,
        request_body,
        response_body,
    }
}

fn xml_body(raw: Option<XmlBody>, fallback_mime: &str, budget: &mut BodyBudget) -> Option<Body> {
    let raw = raw?;
    let mime = if raw.mime.is_empty() {
        fallback_mime.to_string()
    } else {
        raw.mime.clone()
    };
    if raw.total == 0 && raw.declared_size.unwrap_or(0) == 0 {
        return None;
    }
    let encoded = raw
        .encoding
        .as_deref()
        .is_some_and(|encoding| encoding.eq_ignore_ascii_case("base64"));
    if encoded {
        let size = raw.declared_size.unwrap_or_else(|| {
            let padding = raw.padding.min(2);
            (((raw.encoded_chars / 4) * 3).saturating_sub(padding)) as u64
        });
        if raw.preview.is_empty() {
            return Some(Body {
                preview: None,
                size: Some(size),
                truncated: true,
                binary: false,
                bytes: None,
                mime,
            });
        }
        let cap = BODY_PREVIEW_BYTES.min(budget.remaining);
        let Some(mut bytes) = decode_base64_prefix(&raw.preview, cap) else {
            return Some(Body {
                preview: None,
                size: Some(size),
                truncated: true,
                binary: true,
                bytes: None,
                mime,
            });
        };
        let allowed = budget.take(bytes.len());
        bytes.truncate(allowed);
        let truncated = (size as usize) > bytes.len();
        let text = std::str::from_utf8(&bytes)
            .ok()
            .map(|text| text.to_string());
        return Some(match text {
            Some(text) => Body {
                preview: Some(text),
                size: Some(size),
                truncated,
                binary: false,
                bytes: None,
                mime,
            },
            None => Body {
                preview: None,
                size: Some(size),
                truncated,
                binary: true,
                bytes: Some(bytes),
                mime,
            },
        });
    }
    let allowed = budget.take(raw.preview.len());
    Some(Body {
        preview: Some(truncate_chars(&raw.preview, allowed)),
        size: Some(raw.total as u64),
        truncated: allowed < raw.total,
        binary: false,
        bytes: None,
        mime,
    })
}

fn local_name(start: &BytesStart<'_>) -> String {
    start.local_name().as_ref().to_string()
}

fn local_name_end(end: &quick_xml::events::BytesEnd<'_>) -> String {
    end.local_name().as_ref().to_string()
}

fn attr(start: &BytesStart<'_>, name: &str) -> Option<String> {
    start
        .attributes()
        .with_checks(false)
        .flatten()
        .find(|attribute| attribute.key.local_name().as_ref() == name)
        .and_then(|attribute| {
            attribute
                .normalized_value(quick_xml::XmlVersion::Explicit1_0)
                .ok()
        })
        .map(|value| value.into_owned())
}

fn attr_i64(start: &BytesStart<'_>, name: &str) -> Option<i64> {
    attr(start, name).and_then(|value| value.parse().ok())
}

fn attr_u16(start: &BytesStart<'_>, name: &str) -> Option<u16> {
    attr(start, name).and_then(|value| value.parse().ok())
}

fn attr_u64(start: &BytesStart<'_>, name: &str) -> Option<u64> {
    attr(start, name).and_then(|value| value.parse().ok())
}

fn resolve_reference(reference: &str) -> String {
    match reference {
        "amp" => "&".to_string(),
        "lt" => "<".to_string(),
        "gt" => ">".to_string(),
        "quot" => "\"".to_string(),
        "apos" => "'".to_string(),
        _ => {
            if let Some(hex) = reference
                .strip_prefix("#x")
                .or_else(|| reference.strip_prefix("#X"))
            {
                return u32::from_str_radix(hex, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .map(|ch| ch.to_string())
                    .unwrap_or_else(|| format!("&{reference};"));
            }
            if let Some(decimal) = reference.strip_prefix('#') {
                return decimal
                    .parse::<u32>()
                    .ok()
                    .and_then(char::from_u32)
                    .map(|ch| ch.to_string())
                    .unwrap_or_else(|| format!("&{reference};"));
            }
            format!("&{reference};")
        }
    }
}

fn status_from_first_line(line: &str) -> Option<u16> {
    line.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
mod tests;
