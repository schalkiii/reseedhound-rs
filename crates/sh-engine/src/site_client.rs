//! 站点 HTTP 客户端：对齐 Python src/network/client.py 的全部行为语义。
//!
//! - 全局信号量限流 + host 级下载间隔（含指数感知的 backoff 因子）
//! - 限流响应解析（"N分钟后"/"N秒后"文案 → 等待秒数 → 放大 host backoff）
//! - 硬失败短路（401/403/404/405/410、DNS 失败、证书错误不重试）
//! - HTML 错误页检测（登录/权限/删除/404 → 死种标记）
//! - 429 Retry-After 处理
//! - 死种缓存（内存 + 可选持久化回调）

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, Semaphore};

use crate::error::EngineError;

/// 与 Python DEAD_TORRENT_PATTERNS 对齐。
/// 返回原因码（用于日志与死种标记），None 表示未识别。
pub fn detect_html_reason(data: &[u8]) -> Option<&'static str> {
    let text = String::from_utf8_lossy(data);
    let t = text.as_ref();
    // 顺序即优先级，与 Python 列表序一致
    if t.contains("没有权限") || t.contains("无权访问") || t.contains("无权限") {
        return Some("permission_denied");
    }
    let lower = t.to_lowercase();
    if t.contains("种子已删除")
        || t.contains("已被删除")
        || t.contains("已被移除")
        || (lower.contains("torrent") && (lower.contains("deleted") || lower.contains("removed")))
    {
        return Some("torrent_deleted");
    }
    if t.contains("不存在")
        || lower.contains("not found")
        || lower.contains("no such")
        || lower.contains("404")
    {
        return Some("not_found");
    }
    // Python 正则 sign.?in 兼容 sign in / sign-in / signin 三种形式
    if t.contains("未登录")
        || t.contains("请登录")
        || lower.contains("login")
        || lower.contains("sign in")
        || lower.contains("sign-in")
        || lower.contains("signin")
    {
        return Some("auth_required");
    }
    if t.contains("禁止访问")
        || lower.contains("access denied")
        || lower.contains("forbidden")
        || lower.contains("403")
    {
        return Some("forbidden");
    }
    None
}

/// 与 Python RATE_LIMIT_HINT_PATTERNS / RATE_LIMIT_KEYWORDS 对齐。
/// 返回建议等待秒数；None 表示非限流响应。
pub fn parse_rate_limit_response(data: &[u8]) -> Option<f64> {
    if data.len() > 10000 {
        return None;
    }
    let text = String::from_utf8_lossy(data);
    let first = &text[..text.len().min(500)];
    let has_keyword = RATE_LIMIT_KEYWORDS.iter().any(|kw| first.contains(kw));
    if !has_keyword {
        return None;
    }
    if let Some(delay) = extract_hint_delay(first) {
        Some(delay.max(10.0))
    } else {
        Some(60.0)
    }
}

/// Python `_is_likely_text_error`：短文本（<500B）且非 HTML 的限流提示。
pub fn is_likely_text_error(data: &[u8]) -> Option<f64> {
    if data.len() > 500 {
        return None;
    }
    let head = &data[..data.len().min(200)];
    let head_lower: Vec<u8> = head.iter().map(|b| b.to_ascii_lowercase()).collect();
    if head_lower.windows(5).any(|w| w == b"<html")
        || head_lower.windows(9).any(|w| w == b"<!doctype")
    {
        return None;
    }
    let text = String::from_utf8_lossy(data);
    let first = &text[..text.len().min(200)];
    let has_keyword = RATE_LIMIT_KEYWORDS.iter().any(|kw| first.contains(kw));
    if !has_keyword {
        return None;
    }
    if let Some(delay) = extract_hint_delay(first) {
        Some(delay.max(10.0))
    } else {
        Some(60.0)
    }
}

const RATE_LIMIT_KEYWORDS: &[&str] = &["频繁", "rate limit", "too many", "请稍后", "请稍候"];

/// "(\d+)\s*分钟后" / "(\d+)\s*秒后" / "wait N minute(s)" / "try again in N second(s)" / "retry after N second(s)"
fn extract_hint_delay(text: &str) -> Option<f64> {
    // 中文提示：数字 + 单位
    for (pat, is_minute) in [(r"(\d+)\s*分钟后", true), (r"(\d+)\s*秒后", false)] {
        if let Ok(re) = regex::Regex::new(pat) {
            if let Some(caps) = re.captures(text) {
                if let Some(v) = caps.get(1).and_then(|m| m.as_str().parse::<f64>().ok()) {
                    return Some(if is_minute { v * 60.0 } else { v });
                }
            }
        }
    }
    // 英文提示（小写匹配）
    let lower = text.to_lowercase();
    for (pat, is_minute) in [
        (r"wait\s+(\d+)\s+minute", true),
        (r"try\s+again\s+in\s+(\d+)\s+second", false),
        (r"retry\s+after\s+(\d+)\s+second", false),
    ] {
        if let Ok(re) = regex::Regex::new(pat) {
            if let Some(caps) = re.captures(&lower) {
                if let Some(v) = caps.get(1).and_then(|m| m.as_str().parse::<f64>().ok()) {
                    return Some(if is_minute { v * 60.0 } else { v });
                }
            }
        }
    }
    None
}

/// 这些状态码是硬失败，重试不会成功。
const NON_RETRYABLE_HTTP_STATUS: &[u16] = &[401, 403, 404, 405, 410];

/// DNS / 证书类硬失败的错误消息特征（小写子串匹配）。
const NON_RETRYABLE_ERROR_HINTS: &[&str] = &[
    "getaddrinfo failed",
    "name or service not known",
    "certificate verify failed",
    "certificateverifyfailed",
    "clientconnectorcertificateerror",
    "ssl: certificate_verify_failed",
    "dns error",
    "error building cert",
];

pub fn is_non_retryable_error(message: &str) -> bool {
    let low = message.to_lowercase();
    NON_RETRYABLE_ERROR_HINTS.iter().any(|h| low.contains(h))
}

/// 下载失败原因码（与 Python 版字符串对齐，便于日志对照）。
#[derive(Debug, Clone, PartialEq)]
pub enum DownloadReason {
    Ok,
    DeadCached,
    Timeout,
    ConnectionError,
    AuthRedirect,
    RateLimit,
    TooShort,
    /// html_<reason>
    HtmlReason(String),
    /// http_<code>
    HttpStatus(u16),
    /// 重试次数耗尽仍超时
    TimeoutExhausted,
}

impl DownloadReason {
    pub fn as_str(&self) -> String {
        match self {
            DownloadReason::Ok => "ok".into(),
            DownloadReason::DeadCached => "dead_cached".into(),
            DownloadReason::Timeout => "timeout".into(),
            DownloadReason::ConnectionError => "connection_error".into(),
            DownloadReason::AuthRedirect => "auth_redirect".into(),
            DownloadReason::RateLimit => "rate_limit".into(),
            DownloadReason::TooShort => "too_short".into(),
            DownloadReason::HtmlReason(r) => format!("html_{r}"),
            DownloadReason::HttpStatus(s) => format!("http_{s}"),
            DownloadReason::TimeoutExhausted => "timeout".into(),
        }
    }
}

/// 死种缓存：内存集合 + 可选持久化回调。
pub struct DeadTorrentCache {
    dead: Mutex<HashSet<(String, i64)>>,
    persist: Option<Box<dyn Fn(String, i64) + Send + Sync>>,
}

impl DeadTorrentCache {
    pub fn new() -> Self {
        Self {
            dead: Mutex::new(HashSet::new()),
            persist: None,
        }
    }

    pub fn with_persist<F: Fn(String, i64) + Send + Sync + 'static>(f: F) -> Self {
        Self {
            dead: Mutex::new(HashSet::new()),
            persist: Some(Box::new(f)),
        }
    }

    pub async fn is_dead(&self, site: &str, torrent_id: i64) -> bool {
        self.dead
            .lock()
            .await
            .contains(&(site.to_string(), torrent_id))
    }

    pub async fn mark_dead(&self, site: &str, torrent_id: i64) {
        self.dead
            .lock()
            .await
            .insert((site.to_string(), torrent_id));
        if let Some(f) = &self.persist {
            f(site.to_string(), torrent_id);
        }
    }

    /// 启动时灌入持久化数据（Python seed()）。
    pub async fn seed(&self, entries: Vec<(String, i64)>) {
        let mut dead = self.dead.lock().await;
        for e in entries {
            dead.insert(e);
        }
    }

    pub async fn size(&self) -> usize {
        self.dead.lock().await.len()
    }
}

impl Default for DeadTorrentCache {
    fn default() -> Self {
        Self::new()
    }
}

/// SiteClient 参数（对齐 Python SiteClient.__init__ 的默认值）。
#[derive(Debug, Clone)]
pub struct SiteClientConfig {
    pub concurrency: usize,
    pub api_timeout_secs: f64,
    pub download_timeout_secs: f64,
    pub retry_count: u32,
    pub retry_delay_secs: f64,
    pub download_interval_secs: f64,
    /// host -> interval 覆盖
    pub host_intervals: HashMap<String, f64>,
    /// domain -> cookie 字符串
    pub site_cookies: HashMap<String, String>,
}

impl Default for SiteClientConfig {
    fn default() -> Self {
        Self {
            concurrency: 20,
            api_timeout_secs: 15.0,
            download_timeout_secs: 60.0,
            retry_count: 2,
            retry_delay_secs: 5.0,
            download_interval_secs: 0.3,
            host_intervals: HashMap::new(),
            site_cookies: HashMap::new(),
        }
    }
}

/// host 级节流状态（per-host interval + backoff 因子）。
#[derive(Default)]
struct HostThrottle {
    last_request: Option<Instant>,
    backoff: f64,
}

pub struct SiteClient {
    http: reqwest::Client,
    download_http: reqwest::Client,
    semaphore: Arc<Semaphore>,
    config: SiteClientConfig,
    dead_cache: Arc<DeadTorrentCache>,
    host_throttle: Arc<Mutex<HashMap<String, HostThrottle>>>,
}

impl SiteClient {
    pub fn new(
        config: SiteClientConfig,
        dead_cache: Arc<DeadTorrentCache>,
    ) -> Result<Self, EngineError> {
        // force_close 等价：pool_max_idle_per_host(0) 连接用完即关，
        // 规避 PT 站长连接静默断开（PYTHON-INDEX §5.1）
        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .timeout(Duration::from_secs_f64(config.api_timeout_secs))
            .user_agent("SeedHound-RS/1.0")
            .build()?;
        // 下载走独立 client（更长超时），同样 force_close
        let download_http = reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .timeout(Duration::from_secs_f64(config.download_timeout_secs))
            .user_agent("SeedHound-RS/1.0")
            .build()?;
        Ok(Self {
            http,
            download_http,
            semaphore: Arc::new(Semaphore::new(config.concurrency)),
            config,
            dead_cache,
            host_throttle: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn dead_cache(&self) -> &Arc<DeadTorrentCache> {
        &self.dead_cache
    }

    /// host 级下载间隔（对齐 Python get_bytes 的 host lock + backoff 逻辑）。
    async fn wait_host_slot(&self, url: &str) {
        let host = match url::Url::parse(url) {
            Ok(u) => u.host_str().unwrap_or("").to_string(),
            Err(_) => String::new(),
        };
        if host.is_empty() {
            return;
        }
        let interval = self
            .config
            .host_intervals
            .get(&host)
            .copied()
            .unwrap_or(self.config.download_interval_secs);
        if interval <= 0.0 {
            return;
        }
        loop {
            let wait = {
                let mut throttle = self.host_throttle.lock().await;
                let t = throttle.entry(host.clone()).or_default();
                let backoff = if t.backoff > 0.0 { t.backoff } else { 1.0 };
                let need = Duration::from_secs_f64(interval * backoff);
                let Some(last) = t.last_request else {
                    t.last_request = Some(Instant::now());
                    return;
                };
                let elapsed = last.elapsed();
                if elapsed >= need {
                    t.last_request = Some(Instant::now());
                    return;
                }
                need - elapsed
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// 检测到限流后放大该 host 的 backoff（对齐 Python：limit_delay / host_interval）。
    async fn bump_host_backoff(&self, url: &str, limit_delay: f64) {
        let host = match url::Url::parse(url) {
            Ok(u) => u.host_str().unwrap_or("").to_string(),
            Err(_) => String::new(),
        };
        if host.is_empty() {
            return;
        }
        let interval = self
            .config
            .host_intervals
            .get(&host)
            .copied()
            .unwrap_or(self.config.download_interval_secs)
            .max(1.0);
        let factor = (limit_delay / interval).max(1.0);
        let mut throttle = self.host_throttle.lock().await;
        let t = throttle.entry(host).or_default();
        if t.backoff < factor {
            t.backoff = factor;
        }
    }

    /// 单次 GET 下载（对齐 Python get_bytes 全部分支）。
    pub async fn get_bytes(
        &self,
        url: &str,
        site_name: &str,
        torrent_id: i64,
    ) -> (Option<Vec<u8>>, DownloadReason) {
        // host 级节流
        self.wait_host_slot(url).await;

        let _permit = match self.semaphore.acquire().await {
            Ok(p) => p,
            Err(_) => return (None, DownloadReason::ConnectionError),
        };

        let resp = match self.download_http.get(url).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("下载错误 {url}: {e}");
                return (None, DownloadReason::ConnectionError);
            }
        };
        // reqwest 默认跟随重定向。Python 版未 follow，对 redirect 到 login 的场景
        // 标记 auth_redirect。这里检查最终 URL 是否跳到了 login 页。
        let final_url = resp.url().clone();
        let status = resp.status();
        let headers = resp.headers().clone();
        let data = match resp.bytes().await {
            Ok(b) => b.to_vec(),
            Err(e) => {
                tracing::warn!("下载错误 {url}: {e}");
                return (None, DownloadReason::ConnectionError);
            }
        };

        if !status.is_success() {
            tracing::warn!("下载失败 {url}: HTTP {} ", status.as_u16());
            // reqwest 已跟随重定向；若最终落在 login 页面（3xx→login 的语义等价）
            if final_url.as_str().to_lowercase().contains("login") {
                tracing::warn!("  站点 {site_name} 需要登录 Cookie，已标记为 auth_required");
                if !site_name.is_empty() && torrent_id > 0 {
                    self.dead_cache.mark_dead(site_name, torrent_id).await;
                }
                return (None, DownloadReason::AuthRedirect);
            }
            if let Some(delay) = parse_rate_limit_response(&data) {
                tracing::warn!(
                    "  服务器返回限流响应 (HTTP {}, 大小 {})",
                    status.as_u16(),
                    data.len()
                );
                self.bump_host_backoff(url, delay).await;
                return (None, DownloadReason::RateLimit);
            }
            return (None, DownloadReason::HttpStatus(status.as_u16()));
        }

        let head_lower: Vec<u8> = data[..data.len().min(200)]
            .iter()
            .map(|b| b.to_ascii_lowercase())
            .collect();
        let is_html = head_lower.windows(5).any(|w| w == b"<html")
            || head_lower.windows(9).any(|w| w == b"<!doctype");

        if is_html {
            let reason = detect_html_reason(&data);
            tracing::warn!(
                "下载返回HTML而非torrent: {url} (大小: {}, 原因: {})",
                data.len(),
                reason.unwrap_or("未知")
            );
            if let Some(r) = reason {
                if r != "unknown" && !site_name.is_empty() && torrent_id > 0 {
                    self.dead_cache.mark_dead(site_name, torrent_id).await;
                }
                return (None, DownloadReason::HtmlReason(r.to_string()));
            }
            return (None, DownloadReason::HtmlReason("unknown".into()));
        }

        if let Some(delay) = is_likely_text_error(&data) {
            tracing::warn!("下载返回文本错误而非torrent: {url} (大小: {})", data.len());
            self.bump_host_backoff(url, delay).await;
            return (None, DownloadReason::RateLimit);
        }

        if data.is_empty() || data.len() < 20 {
            tracing::warn!("下载数据过短 ({} 字节): {url}", data.len());
            return (None, DownloadReason::TooShort);
        }

        let _ = headers; // Content-Type 仅用于 Python 日志；Rust 版语义等价
        (Some(data), DownloadReason::Ok)
    }

    /// 带重试的下载（对齐 Python try_download：仅 timeout 触发重试）。
    pub async fn try_download(
        &self,
        url: &str,
        site_name: &str,
        torrent_id: i64,
    ) -> (Option<Vec<u8>>, DownloadReason) {
        if !site_name.is_empty()
            && torrent_id > 0
            && self.dead_cache.is_dead(site_name, torrent_id).await
        {
            return (None, DownloadReason::DeadCached);
        }

        for attempt in 0..=self.config.retry_count {
            let (data, reason) = self.get_bytes(url, site_name, torrent_id).await;
            if data.is_some() {
                if attempt > 0 {
                    tracing::info!("下载成功 {url} (第 {attempt} 次重试后恢复)");
                }
                return (data, DownloadReason::Ok);
            }
            if reason == DownloadReason::Timeout && attempt < self.config.retry_count {
                let delay = self.config.retry_delay_secs * (attempt as f64 + 1.0);
                tracing::debug!(
                    "下载超时 {url}，等待 {delay:.1} 秒后重试 ({}/{})",
                    attempt + 1,
                    self.config.retry_count
                );
                tokio::time::sleep(Duration::from_secs_f64(delay)).await;
            } else {
                return (None, reason);
            }
        }
        (None, DownloadReason::TimeoutExhausted)
    }

    /// POST JSON（对齐 Python post_json：429 处理、硬失败短路、重试计数）。
    pub async fn post_json(
        &self,
        url: &str,
        body: &serde_json::Value,
        headers: Option<&reqwest::header::HeaderMap>,
        retries: u32,
        label: &str,
    ) -> Option<serde_json::Value> {
        let suffix = if label.is_empty() {
            String::new()
        } else {
            format!(" [{label}]")
        };
        let mut last_error = String::new();
        // 首次失败原因：重试恢复时一并打出，便于定位超时/限流（否则只有 DEBUG 可见）
        let mut first_error = String::new();
        let mut attempts = 0u32;

        for attempt in 0..retries.max(1) {
            attempts = attempt + 1;
            {
                let _permit = match self.semaphore.acquire().await {
                    Ok(p) => p,
                    Err(_) => return None,
                };
                let mut req = self.http.post(url).json(body);
                if let Some(h) = headers {
                    req = req.headers(h.clone());
                }
                match req.send().await {
                    Ok(resp) => {
                        let status = resp.status();
                        if status.is_success() {
                            match resp.json::<serde_json::Value>().await {
                                Ok(v) => {
                                    if attempt > 0 {
                                        tracing::info!(
                                            "请求成功 {url}{suffix} (第 {attempt} 次重试后恢复，首次错误: {first_error})"
                                        );
                                    }
                                    return Some(v);
                                }
                                Err(_) => {
                                    // HTTP 200 但非 JSON：表单错误页/登录跳转，不重试
                                    tracing::warn!(
                                        "请求失败 {url}{suffix} (html_invalid_response: HTTP 200 但返回非 JSON，可能是表单错误页或需登录，不重试)"
                                    );
                                    return None;
                                }
                            }
                        }
                        let code = status.as_u16();
                        if code == 429 {
                            let retry_after = resp
                                .headers()
                                .get("Retry-After")
                                .and_then(|v| v.to_str().ok())
                                .and_then(|v| v.parse::<f64>().ok())
                                .unwrap_or(5.0);
                            tokio::time::sleep(Duration::from_secs_f64(retry_after)).await;
                            continue;
                        }
                        last_error = format!("HTTP {code}");
                        if first_error.is_empty() {
                            first_error = last_error.clone();
                        }
                        if NON_RETRYABLE_HTTP_STATUS.contains(&code) {
                            tracing::warn!("请求失败 {url}{suffix} ({last_error}，硬失败不重试)");
                            return None;
                        }
                        tracing::debug!(
                            "{last_error} from {url}{suffix} (attempt {}/{retries})",
                            attempt + 1
                        );
                    }
                    Err(e) => {
                        last_error = e.to_string();
                        if first_error.is_empty() {
                            first_error = last_error.clone();
                        }
                        if is_non_retryable_error(&last_error) {
                            tracing::warn!("请求失败 {url}{suffix} ({last_error}，硬失败不重试)");
                            return None;
                        }
                        tracing::debug!(
                            "请求错误 {url}: {last_error}{suffix} (attempt {}/{retries})",
                            attempt + 1
                        );
                    }
                }
            }
            if attempt + 1 < retries.max(1) {
                let delay = self.config.retry_delay_secs * (attempt as f64 + 1.0);
                tokio::time::sleep(Duration::from_secs_f64(delay)).await;
            }
        }
        tracing::warn!("请求失败 {url}{suffix} ({last_error}, 共尝试 {attempts} 次)");
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_reason_detection_matches_python_matrix() {
        assert_eq!(
            detect_html_reason("对不起，您没有权限下载".as_bytes()),
            Some("permission_denied")
        );
        assert_eq!(
            detect_html_reason("该种子已删除".as_bytes()),
            Some("torrent_deleted")
        );
        assert_eq!(
            detect_html_reason(b"Torrent was removed by uploader"),
            Some("torrent_deleted")
        );
        assert_eq!(
            detect_html_reason("资源不存在".as_bytes()),
            Some("not_found")
        );
        assert_eq!(
            detect_html_reason("请登录后下载".as_bytes()),
            Some("auth_required")
        );
        // Python 正则 `请登录` 为连续匹配："请先登录"（中间隔字）两边都不命中
        assert_eq!(detect_html_reason("请先登录".as_bytes()), None);
        assert_eq!(detect_html_reason(b"Access Denied"), Some("forbidden"));
        assert_eq!(detect_html_reason("正常页面内容".as_bytes()), None);
    }

    #[test]
    fn rate_limit_parsing_matches_python() {
        // 分钟提示
        let d = parse_rate_limit_response("请求过于频繁，请 5 分钟后重试".as_bytes());
        assert_eq!(d, Some(300.0));
        // 秒提示
        let d = parse_rate_limit_response("操作频繁，请10秒后再试".as_bytes());
        assert_eq!(d, Some(10.0));
        // 有关键词无数字 → 默认 60s
        let d = parse_rate_limit_response("请求过于频繁".as_bytes());
        assert_eq!(d, Some(60.0));
        // 无关键词 → None
        assert_eq!(
            parse_rate_limit_response("normal response".as_bytes()),
            None
        );
        // 英文（关键词大小写敏感，与 Python `in` 一致；"Too many" 大写 T 不命中）
        let d = parse_rate_limit_response(b"too many requests, wait 2 minutes");
        assert_eq!(d, Some(120.0));
        assert_eq!(parse_rate_limit_response(b"Too many requests"), None);
    }

    #[test]
    fn text_error_detection() {
        // 短文本 + 关键词
        let d = is_likely_text_error("rate limit: try again in 30 seconds".as_bytes());
        assert_eq!(d, Some(30.0));
        // HTML 头 → None（交给 html 检测）
        let d = is_likely_text_error(b"<html>too many requests</html>");
        assert_eq!(d, None);
        // 超长 → None
        let long = vec![b'a'; 600];
        assert_eq!(is_likely_text_error(&long), None);
    }

    #[tokio::test]
    async fn dead_cache_mark_and_query() {
        let cache = DeadTorrentCache::new();
        cache.mark_dead("MTeam", 123).await;
        assert!(cache.is_dead("MTeam", 123).await);
        assert!(!cache.is_dead("MTeam", 124).await);
        assert!(!cache.is_dead("Other", 123).await);
        cache.seed(vec![("HDSky".into(), 9)]).await;
        assert!(cache.is_dead("HDSky", 9).await);
        assert_eq!(cache.size().await, 2);
    }

    #[tokio::test]
    async fn host_throttle_enforces_interval() {
        // 间隔 0.2s：两次请求间应至少间隔 0.2s
        let config = SiteClientConfig {
            download_interval_secs: 0.2,
            ..Default::default()
        };
        let client = SiteClient::new(config, Arc::new(DeadTorrentCache::new())).unwrap();
        let start = Instant::now();
        client.wait_host_slot("https://example.com/a").await;
        client.wait_host_slot("https://example.com/b").await;
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(190), "elapsed {elapsed:?}");
    }
}
