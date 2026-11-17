//! HDArea REST API v1 info_hash 批量匹配通道。
//!
//! 对齐 Python src/network/hdarea.py：
//! - POST /api/v1/torrents/hashes + X-API-Key，单批 ≤200
//! - 响应统一包裹 success/code/data.items
//! - 请求间隔节流（站点限流 60 次/分）

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::json;
use tokio::sync::{Mutex, Semaphore};

use crate::site_client::SiteClient;

pub const HDAREA_API_TYPE: &str = "hdarea";
pub const HDAREA_HASHES_PATH: &str = "/api/v1/torrents/hashes";
pub const MAX_HASHES_PER_REQUEST: usize = 200;

#[derive(Debug, Clone, PartialEq)]
pub struct HdareaMatch {
    pub info_hash: String,
    pub torrent_id: i64,
    pub download_url: String,
}

pub fn is_hdarea_site(api_type: &str) -> bool {
    api_type.eq_ignore_ascii_case(HDAREA_API_TYPE)
}

/// 优先显式 api_url，否则由站点主页推导官方 REST 路径。
pub fn hashes_url(api_url: &str, site_url: &str) -> String {
    let configured = api_url.trim_end_matches('/');
    if !configured.is_empty() {
        return configured.to_string();
    }
    format!("{}{}", site_url.trim_end_matches('/'), HDAREA_HASHES_PATH)
}

pub fn is_valid_info_hash(h: &str) -> bool {
    h.len() == 40 && h.chars().all(|c| c.is_ascii_hexdigit())
}

/// 过滤非法 info_hash 并按上限分批。
pub fn split_batches(info_hashes: &[String], size: usize) -> Vec<Vec<String>> {
    let valid: Vec<String> = info_hashes
        .iter()
        .filter(|h| is_valid_info_hash(h))
        .map(|h| h.to_lowercase())
        .collect();
    let limit = size.clamp(1, MAX_HASHES_PER_REQUEST);
    valid.chunks(limit).map(|c| c.to_vec()).collect()
}

#[derive(Debug, Deserialize)]
struct HashesResponseItem {
    #[serde(default)]
    info_hash: String,
    id: serde_json::Value,
    #[serde(default)]
    download_url: String,
}

#[derive(Debug, Deserialize)]
struct HashesResponse {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    code: i64,
    data: Option<HashesData>,
}

#[derive(Debug, Deserialize)]
struct HashesData {
    #[serde(default)]
    items: Vec<HashesResponseItem>,
}

/// 解析统一包裹结构，只保留字段完整的命中项。
pub fn parse_hashes_response(response: Option<&serde_json::Value>) -> Vec<HdareaMatch> {
    let Some(resp) = response else {
        return vec![];
    };
    let Ok(parsed) = serde_json::from_value::<HashesResponse>(resp.clone()) else {
        return vec![];
    };
    if !parsed.success || parsed.code != 0 {
        return vec![];
    }
    let Some(data) = parsed.data else {
        return vec![];
    };
    let mut matches = Vec::new();
    for item in data.items {
        let info_hash = item.info_hash.to_lowercase();
        if !is_valid_info_hash(&info_hash) {
            continue;
        }
        // Python int() 宽容字符串数字；JSON 数字直接取
        let torrent_id = match &item.id {
            serde_json::Value::Number(n) => n.as_i64(),
            serde_json::Value::String(s) => s.parse::<i64>().ok(),
            _ => None,
        };
        let Some(torrent_id) = torrent_id else {
            continue;
        };
        matches.push(HdareaMatch {
            info_hash,
            torrent_id,
            download_url: item.download_url,
        });
    }
    matches
}

/// HDArea 批量匹配器（请求间隔节流 + 批并发限制）。
pub struct HdareaMatcher {
    client: Arc<SiteClient>,
    request_interval: Duration,
    retry_count: u32,
    semaphore: Arc<Semaphore>,
    last_request: Mutex<Option<Instant>>,
}

impl HdareaMatcher {
    pub fn new(
        client: Arc<SiteClient>,
        request_interval_secs: f64,
        concurrency: usize,
        retry_count: u32,
    ) -> Self {
        Self {
            client,
            request_interval: Duration::from_secs_f64(request_interval_secs.max(0.0)),
            retry_count: retry_count.max(1),
            semaphore: Arc::new(Semaphore::new(concurrency.max(1))),
            last_request: Mutex::new(None),
        }
    }

    async fn throttle(&self) {
        if self.request_interval.is_zero() {
            return;
        }
        let mut last = self.last_request.lock().await;
        if let Some(prev) = *last {
            let elapsed = prev.elapsed();
            if elapsed < self.request_interval {
                tokio::time::sleep(self.request_interval - elapsed).await;
            }
        }
        *last = Some(Instant::now());
    }

    pub async fn query(
        &self,
        site_name: &str,
        api_url: &str,
        site_url: &str,
        api_key: &str,
        info_hashes: &[String],
    ) -> Vec<HdareaMatch> {
        if api_key.is_empty() {
            tracing::warn!("站点 {site_name} 缺少 api_key，跳过 HDArea 通道");
            return vec![];
        }
        let url = hashes_url(api_url, site_url);
        let batches = split_batches(info_hashes, MAX_HASHES_PER_REQUEST);
        if batches.is_empty() {
            return vec![];
        }

        let mut headers = reqwest::header::HeaderMap::new();
        if let Ok(v) = reqwest::header::HeaderValue::from_str(api_key) {
            headers.insert("X-API-Key", v);
        }

        let mut matches = Vec::new();
        let mut failed_batches = 0usize;
        for (i, batch) in batches.iter().enumerate() {
            let label = format!("第 {}/{} 批", i + 1, batches.len());
            let permit = match self.semaphore.acquire().await {
                Ok(p) => p,
                Err(_) => break,
            };
            self.throttle().await;
            let body = json!({ "hashes": batch });
            let response = self
                .client
                .post_json(&url, &body, Some(&headers), self.retry_count, &label)
                .await;
            drop(permit);
            let parsed = parse_hashes_response(response.as_ref());
            if parsed.is_empty() && response.is_none() {
                failed_batches += 1;
            }
            matches.extend(parsed);
        }
        tracing::info!(
            "站点 {site_name} 查询完成: {} 批 / 失败 {failed_batches} 批 / 命中 {} 个种子",
            batches.len(),
            matches.len()
        );
        matches
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_derivation() {
        assert_eq!(
            hashes_url("", "https://hdarea.co/"),
            "https://hdarea.co/api/v1/torrents/hashes"
        );
        assert_eq!(
            hashes_url("https://custom/api", "https://hdarea.co"),
            "https://custom/api"
        );
    }

    #[test]
    fn split_batches_filters_and_chunks() {
        let hashes: Vec<String> = vec![
            "G".repeat(40),                                    // 无效（G 非 hex）
            "0123456789abcdef0123456789abcdef01234567".into(), // 有效
            "ABCDEF0123456789ABCDEF0123456789ABCDEF01".into(), // 有效（大写→小写）
        ];
        let batches = split_batches(&hashes, 200);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 2);
        assert_eq!(batches[0][1], "abcdef0123456789abcdef0123456789abcdef01");
        // 分批：size=1 时每批 1 个
        let batches = split_batches(&hashes, 1);
        assert_eq!(batches.len(), 2);
    }

    #[test]
    fn parse_response_matches_python_semantics() {
        // 成功响应
        let ok = serde_json::json!({
            "success": true, "code": 0,
            "data": {"count": 2, "items": [
                {"id": 460345, "info_hash": "0123456789abcdef0123456789abcdef01234567", "name": "a", "size": 1, "download_url": "https://x/dl?id=1"},
                {"id": "bad", "info_hash": "0123456789abcdef0123456789abcdef01234568", "download_url": ""},
                {"id": 2, "info_hash": "short", "download_url": ""}
            ]}
        });
        let m = parse_hashes_response(Some(&ok));
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].torrent_id, 460345);
        assert_eq!(m[0].info_hash, "0123456789abcdef0123456789abcdef01234567");

        // 失败包裹
        let fail = serde_json::json!({"success": false, "code": 1});
        assert!(parse_hashes_response(Some(&fail)).is_empty());
        // None
        assert!(parse_hashes_response(None).is_empty());
        // id 是字符串数字也应通过（Python int() 宽容）
        let str_id = serde_json::json!({
            "success": true, "code": 0,
            "data": {"items": [{"id": "123", "info_hash": "0123456789abcdef0123456789abcdef01234567"}]}
        });
        let m = parse_hashes_response(Some(&str_id));
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].torrent_id, 123);
    }
}
