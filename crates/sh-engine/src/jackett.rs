//! Jackett 集成：torznab 搜索 + 尺寸过滤 + pieces_hash 校验下载。
//!
//! 对齐 Python src/engine/jackett.py：
//! - `_extract_search_terms`：清理分隔符 → 截技术标签 → 取年份
//! - torznab XML 解析（title/size/link/guid/attrs.seeders/peers/infohash）
//! - `filter_by_size`：±10MB(≥1GB) / ±10KB(≥1MB) / ±1B 容差
//! - 搜索间隔节流 + 重试
//! - 下载缓存（url → pieces_hash，避免重复下载不匹配项）

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::site_client::SiteClient;
use sh_core::bencode::bdecode;
use sh_core::bencode::BencodeRef;
use sh_core::hash::sha1_hex;

/// 技术标签截断：命中则其后内容视为规格信息，不进搜索词。
/// 与 Python _TECH_TAGS 正则语义一致（单词边界 + 大小写不敏感）。
pub fn extract_search_terms(torrent_name: &str) -> String {
    let cleaned = torrent_name.replace(['.', '_'], " ");
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");

    // 年份：首个 19xx / 20xx
    let year = find_year(&cleaned);
    let before_year = match year {
        Some((start, _)) => &cleaned[..start],
        None => cleaned.as_str(),
    };

    let words: Vec<&str> = before_year.split_whitespace().collect();
    let tech_start = words.iter().position(|w| is_tech_tag(w));
    let title_words: Vec<&str> = match tech_start {
        Some(i) => words[..i].to_vec(),
        None => words.clone(),
    };
    let title_words = if title_words.is_empty() {
        vec![before_year.trim()]
    } else {
        title_words
    };

    let mut parts = vec![title_words.join(" ")];
    if let Some((_, y)) = year {
        parts.push(y.to_string());
    }
    let mut term = parts.join(" ").trim().to_string();
    if term.len() > 120 {
        // 对齐 Python [:120].rsplit(' ',1)[0]
        let head = &term[..120.min(term.len())];
        term = match head.rfind(' ') {
            Some(i) => head[..i].to_string(),
            None => head.to_string(),
        };
    }
    term
}

/// 找首个独立年份词（19xx/20xx），返回 (起始字节, 年份)。
fn find_year(text: &str) -> Option<(usize, &str)> {
    for (idx, word) in text.split(|c: char| c.is_whitespace()).enumerate() {
        if (word.starts_with("19") || word.starts_with("20"))
            && word.len() == 4
            && word.chars().all(|c| c.is_ascii_digit())
        {
            // 计算字节偏移
            let byte_off = text
                .split_whitespace()
                .take(idx)
                .map(|w| w.len() + 1)
                .sum::<usize>();
            return Some((byte_off.min(text.len()), word));
        }
    }
    None
}

const TECH_TAG_EXACT: &[&str] = &[
    "1080p", "1080i", "720p", "2160p", "4k", "480p", "576p", "bluray", "blu-ray", "web-dl",
    "webrip", "hdtv", "dvdrip", "bdrip", "remux", "amzn", "nf", "h.264", "h264", "h.265", "h265",
    "hevc", "avc", "xvid", "divx", "x264", "x265", "10bit", "8bit", "dv", "dovi", "sdr", "hlg",
    "mnhd", "frds", "hhweb", "djweb", "nogroup", "no1", "trinity", "flux",
];

/// 技术标签判定（词级匹配，大小写不敏感）。
/// Python 正则用 \b 边界 + 前缀式匹配（DDP[\.\d]* / DTS[\-\.\w]*），
/// 这里对音频标签做前缀匹配，其余精确匹配。
fn is_tech_tag(word: &str) -> bool {
    let w = word.to_lowercase();
    if TECH_TAG_EXACT.contains(&w.as_str()) {
        return true;
    }
    // 音频前缀族：DDP / DD / DTS / Atmos / TrueHD / FLAC / AAC / AC3
    if w.starts_with("ddp") || w.starts_with("dd") || w.starts_with("dts") {
        return true;
    }
    if w.starts_with("atmos")
        || w.starts_with("truehd")
        || w.starts_with("flac")
        || w.starts_with("aac")
        || w.starts_with("ac3")
    {
        return true;
    }
    // HDR / HDR10+ / HDRDV
    if w.starts_with("hdr") {
        return true;
    }
    false
}

#[derive(Debug, Clone, PartialEq)]
pub struct JackettResult {
    pub title: String,
    pub size: i64,
    pub seeders: i64,
    pub peers: i64,
    pub download_url: String,
    pub info_hash: String,
    pub indexer_id: String,
    pub indexer_name: String,
    pub publish_date: String,
}

#[derive(Debug, Default, Clone)]
pub struct JackettStats {
    pub searched_count: u64,
    pub matched_count: u64,
    pub downloaded_count: u64,
    pub failed_count: u64,
    pub indexer_hits: HashMap<String, u64>,
}

/// torznab XML 解析（与 Python _parse_torznab 对齐）。
pub fn parse_torznab(xml_text: &str) -> Vec<JackettResult> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut results = Vec::new();
    let mut reader = Reader::from_str(xml_text);
    reader.config_mut().trim_text(true);

    let mut depth_in_item = 0usize;
    let mut current_tag = String::new();
    let mut current_text = String::new();

    let mut title = String::new();
    let mut size: i64 = 0;
    let mut link = String::new();
    let mut guid = String::new();
    let mut indexer_id = String::new();
    let mut indexer_name = String::new();
    let mut pubdate = String::new();
    let mut seeders: i64 = 0;
    let mut peers: i64 = 0;
    let mut tn_infohash = String::new();
    let mut in_attrs = false;

    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let local = local_name(&name);
                match local.as_str() {
                    "item" => {
                        depth_in_item += 1;
                        // reset
                        title.clear();
                        size = 0;
                        link.clear();
                        guid.clear();
                        indexer_id.clear();
                        indexer_name.clear();
                        pubdate.clear();
                        seeders = 0;
                        peers = 0;
                        tn_infohash.clear();
                    }
                    "attr" if depth_in_item > 0 => {
                        // torznab:attr name="seeders" value="N"
                        let mut attr_name = String::new();
                        let mut attr_value = String::new();
                        for a in e.attributes().flatten() {
                            match a.key.as_ref() {
                                b"name" => {
                                    attr_name = String::from_utf8_lossy(&a.value).into_owned()
                                }
                                b"value" => {
                                    attr_value = String::from_utf8_lossy(&a.value).into_owned()
                                }
                                _ => {}
                            }
                        }
                        match attr_name.as_str() {
                            "seeders" => seeders = attr_value.parse().unwrap_or(0),
                            "peers" => peers = attr_value.parse().unwrap_or(0),
                            "infohash" => tn_infohash = attr_value,
                            _ => {}
                        }
                    }
                    _ => {}
                }
                if local == "attrs" {
                    in_attrs = true;
                }
                if depth_in_item > 0 && local != "item" && local != "attr" {
                    current_tag = local.clone();
                    current_text.clear();
                }
                if local == "jackettindexer" && depth_in_item > 0 {
                    for a in e.attributes().flatten() {
                        if a.key.as_ref() == b"id" {
                            indexer_id = String::from_utf8_lossy(&a.value).into_owned();
                        }
                    }
                }
                let _ = in_attrs;
            }
            Ok(Event::Text(t)) => {
                if depth_in_item > 0 && !current_tag.is_empty() {
                    current_text.push_str(&String::from_utf8_lossy(&t));
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let local = local_name(&name);
                if local == "item" && depth_in_item > 0 {
                    depth_in_item -= 1;
                    let title_trim = title.trim();
                    if !title_trim.is_empty() {
                        let info_hash = if !tn_infohash.is_empty() {
                            tn_infohash.clone()
                        } else {
                            guid.trim().to_string()
                        };
                        results.push(JackettResult {
                            title: title_trim.to_string(),
                            size,
                            seeders,
                            peers,
                            download_url: link.trim().to_string(),
                            info_hash,
                            indexer_id: indexer_id.clone(),
                            indexer_name: indexer_name.trim().to_string(),
                            publish_date: pubdate.trim().to_string(),
                        });
                    }
                } else if depth_in_item > 0 {
                    match local.as_str() {
                        "title" => title = current_text.clone(),
                        "size" => size = current_text.trim().parse().unwrap_or(0),
                        "link" => link = current_text.clone(),
                        "guid" => guid = current_text.clone(),
                        "jackettindexer" => indexer_name = current_text.clone(),
                        "pubDate" => pubdate = current_text.clone(),
                        _ => {}
                    }
                    current_tag.clear();
                }
                if local == "attrs" {
                    in_attrs = false;
                }
            }
            Ok(Event::Empty(e)) => {
                // 自闭合标签（如 <torznab:attr name="seeders" value="5"/>）
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let local = local_name(&name);
                if local == "attr" && depth_in_item > 0 {
                    let mut attr_name = String::new();
                    let mut attr_value = String::new();
                    for a in e.attributes().flatten() {
                        match a.key.as_ref() {
                            b"name" => attr_name = String::from_utf8_lossy(&a.value).into_owned(),
                            b"value" => attr_value = String::from_utf8_lossy(&a.value).into_owned(),
                            _ => {}
                        }
                    }
                    match attr_name.as_str() {
                        "seeders" => seeders = attr_value.parse().unwrap_or(0),
                        "peers" => peers = attr_value.parse().unwrap_or(0),
                        "infohash" => tn_infohash = attr_value,
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return results,
            _ => {}
        }
        buf.clear();
    }
    results
}

fn local_name(name: &str) -> String {
    match name.split_once(':') {
        Some((_, local)) => local.to_string(),
        None => name.to_string(),
    }
}

/// 尺寸过滤（对齐 Python filter_by_size）。
pub fn filter_by_size(results: Vec<JackettResult>, target_size: i64) -> Vec<JackettResult> {
    if target_size <= 0 {
        return vec![];
    }
    let tolerance: i64 = if target_size >= 1_000_000_000 {
        10_000_000
    } else if target_size >= 1_000_000 {
        10_000
    } else {
        1
    };
    let lower = target_size - tolerance;
    let upper = target_size + tolerance;
    results
        .into_iter()
        .filter(|r| lower <= r.size && r.size <= upper)
        .collect()
}

/// 从下载的 torrent 字节提取 pieces_hash（对齐 Python _parse_pieces_hash）。
pub fn parse_pieces_hash(torrent_data: &[u8]) -> Option<String> {
    let torrent = bdecode(torrent_data).ok()?;
    let BencodeRef::Dict(root, _) = &torrent else {
        return None;
    };
    let info = root.get(b"info".as_slice())?;
    let BencodeRef::Dict(info_map, _) = info else {
        return None;
    };
    match info_map.get(b"pieces".as_slice())? {
        BencodeRef::Bytes(b) => Some(sha1_hex(b)),
        _ => None,
    }
}

pub struct JackettClient {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
    search_interval: Duration,
    retry_count: u32,
    retry_delay: f64,
    last_search: Mutex<Option<Instant>>,
    dl_cache: Mutex<HashMap<String, String>>,
    cache_path: PathBuf,
    pub stats: Mutex<JackettStats>,
}

impl JackettClient {
    pub fn new(
        base_url: &str,
        api_key: &str,
        search_interval_secs: f64,
        retry_count: u32,
        retry_delay_secs: f64,
    ) -> Self {
        let mut base = base_url.trim_end_matches('/').to_string();
        if !base.ends_with("/api/v2.0") && !base.contains("/api") {
            base.push_str("/api/v2.0");
        }
        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest client");
        Self {
            base_url: base,
            api_key: api_key.to_string(),
            http,
            search_interval: Duration::from_secs_f64(search_interval_secs.max(0.0)),
            retry_count,
            retry_delay: retry_delay_secs,
            last_search: Mutex::new(None),
            dl_cache: Mutex::new(HashMap::new()),
            cache_path: PathBuf::from("data/jackett_dl_cache.json"),
            stats: Mutex::new(JackettStats::default()),
        }
    }

    pub async fn load_cache(&self) {
        if let Ok(text) = tokio::fs::read_to_string(&self.cache_path).await {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(map) = v.get("pieces_hash").and_then(|m| m.as_object()) {
                    let mut cache = self.dl_cache.lock().await;
                    for (k, val) in map {
                        if let Some(s) = val.as_str() {
                            cache.insert(k.clone(), s.to_string());
                        }
                    }
                    tracing::debug!("Jackett 下载缓存已加载: {} 条", cache.len());
                }
            }
        }
    }

    pub async fn save_cache(&self) {
        let cache = self.dl_cache.lock().await;
        if let Some(parent) = self.cache_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let mut obj = serde_json::Map::new();
        for (k, v) in cache.iter() {
            obj.insert(k.clone(), serde_json::Value::String(v.clone()));
        }
        let _ = tokio::fs::write(
            &self.cache_path,
            serde_json::to_string_pretty(&serde_json::json!({ "pieces_hash": obj }))
                .unwrap_or_default(),
        )
        .await;
    }

    /// torznab 搜索（带间隔节流与重试）。
    pub async fn search(&self, query: &str) -> Vec<JackettResult> {
        // 节流
        {
            let mut last = self.last_search.lock().await;
            if let Some(prev) = *last {
                let elapsed = prev.elapsed();
                if elapsed < self.search_interval {
                    tokio::time::sleep(self.search_interval - elapsed).await;
                }
            }
            *last = Some(Instant::now());
        }

        let url = format!("{}/indexers/all/results/torznab/api", self.base_url);
        for attempt in 0..=self.retry_count {
            let resp = self
                .http
                .get(&url)
                .query(&[
                    ("apikey", self.api_key.as_str()),
                    ("t", "search"),
                    ("q", query),
                ])
                .send()
                .await;
            match resp {
                Ok(r) if r.status().is_success() => {
                    if let Ok(text) = r.text().await {
                        return parse_torznab(&text);
                    }
                }
                Ok(r) => {
                    tracing::debug!(
                        "Jackett 搜索 HTTP {}: {}",
                        r.status(),
                        &query[..query.len().min(50)]
                    );
                    return vec![];
                }
                Err(_) => {
                    tracing::debug!(
                        "Jackett 搜索错误: {} (attempt {})",
                        &query[..query.len().min(50)],
                        attempt + 1
                    );
                }
            }
            if attempt < self.retry_count {
                tokio::time::sleep(Duration::from_secs_f64(
                    self.retry_delay * (attempt as f64 + 1.0),
                ))
                .await;
            }
        }
        vec![]
    }

    /// 搜索 + 尺寸过滤 + 下载校验（对齐 Python search_and_download，返回首个匹配项）。
    pub async fn search_and_download(
        &self,
        torrent_name: &str,
        torrent_size: i64,
        target_pieces_hash: &str,
        http_client: &SiteClient,
    ) -> Option<(Vec<u8>, JackettResult)> {
        if torrent_name.is_empty() || target_pieces_hash.is_empty() {
            return None;
        }
        let query = extract_search_terms(torrent_name);
        if query.is_empty() {
            return None;
        }
        let results = self.search(&query).await;
        self.stats.lock().await.searched_count += 1;

        if results.is_empty() {
            tracing::info!(
                "  [Jackett] 搜索=0 条: {}",
                &torrent_name[..torrent_name.len().min(40)]
            );
            return None;
        }
        let matched = filter_by_size(results, torrent_size);
        if matched.is_empty() {
            tracing::info!(
                "  [Jackett] 尺寸匹配=0 条: {}",
                &torrent_name[..torrent_name.len().min(40)]
            );
            return None;
        }
        {
            let mut stats = self.stats.lock().await;
            stats.matched_count += matched.len() as u64;
        }

        for result in matched {
            if result.download_url.is_empty() {
                continue;
            }
            let url = result.download_url.clone();

            // 缓存：不匹配记录 → 跳过
            {
                let cache = self.dl_cache.lock().await;
                if let Some(cached) = cache.get(&url) {
                    if cached == target_pieces_hash {
                        tracing::info!(
                            "  [Jackett] 缓存命中(hash匹配)! {} -> {}",
                            &torrent_name[..torrent_name.len().min(40)],
                            result.indexer_name
                        );
                    } else {
                        tracing::debug!(
                            "  [Jackett] 缓存命中(跳过): {} -> {} 已记录不匹配",
                            &torrent_name[..torrent_name.len().min(40)],
                            result.indexer_name
                        );
                        continue;
                    }
                }
            }

            if !result.indexer_name.is_empty() {
                let mut stats = self.stats.lock().await;
                *stats
                    .indexer_hits
                    .entry(result.indexer_name.clone())
                    .or_insert(0) += 1;
            }

            let (data, _reason) = http_client
                .try_download(&url, &result.indexer_name, 0)
                .await;
            let Some(torrent_data) = data else {
                self.stats.lock().await.failed_count += 1;
                continue;
            };

            let downloaded_hash = parse_pieces_hash(&torrent_data).unwrap_or_default();
            {
                let mut cache = self.dl_cache.lock().await;
                cache.insert(url, downloaded_hash.clone());
            }
            if downloaded_hash.is_empty() {
                self.stats.lock().await.failed_count += 1;
                continue;
            }
            if downloaded_hash != target_pieces_hash {
                self.stats.lock().await.failed_count += 1;
                continue;
            }
            self.stats.lock().await.downloaded_count += 1;
            tracing::info!(
                "  [Jackett] *** pieces_hash 匹配! *** {} -> {}",
                &torrent_name[..torrent_name.len().min(40)],
                result.indexer_name
            );
            return Some((torrent_data, result));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_terms_extraction_matches_python() {
        // 基本截断：技术标签后不进搜索词
        assert_eq!(
            extract_search_terms("The.Matrix.1999.1080p.BluRay.x264"),
            "The Matrix 1999"
        );
        // 无年份无标签：全名
        assert_eq!(extract_search_terms("Some.Show.S01"), "Some Show S01");
        // 下划线分隔
        assert_eq!(
            extract_search_terms("Movie_Name_2020_WEB-DL"),
            "Movie Name 2020"
        );
        // 空名保护
        assert_eq!(extract_search_terms(""), "");
    }

    #[test]
    fn torznab_parsing_full_item() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<RSS>
  <channel>
    <item>
      <title>Show.S01.1080p</title>
      <size>1234567890</size>
      <link>https://jackett/dl/abc</link>
      <guid>abcdef0123456789abcdef0123456789abcdef01</guid>
      <pubDate>Mon, 01 Jan 2024 00:00:00 GMT</pubDate>
      <jackettindexer id="idx1">Indexer One</jackettindexer>
      <torznab:attr name="seeders" value="5"/>
      <torznab:attr name="peers" value="10"/>
      <torznab:attr name="infohash" value="fedcba0123456789fedcba0123456789fedcba01"/>
    </item>
    <item>
      <title></title>
    </item>
  </channel>
</RSS>"#;
        let results = parse_torznab(xml);
        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert_eq!(r.title, "Show.S01.1080p");
        assert_eq!(r.size, 1234567890);
        assert_eq!(r.download_url, "https://jackett/dl/abc");
        // torznab:attr infohash 优先于 guid
        assert_eq!(r.info_hash, "fedcba0123456789fedcba0123456789fedcba01");
        assert_eq!(r.seeders, 5);
        assert_eq!(r.peers, 10);
        assert_eq!(r.indexer_name, "Indexer One");
        assert_eq!(r.indexer_id, "idx1");
    }

    #[test]
    fn size_filter_tolerances() {
        let mk = |size: i64| JackettResult {
            title: "t".into(),
            size,
            seeders: 0,
            peers: 0,
            download_url: "u".into(),
            info_hash: String::new(),
            indexer_id: String::new(),
            indexer_name: String::new(),
            publish_date: String::new(),
        };
        // GB 级 ±10MB
        let target = 2_000_000_000i64;
        let results = vec![mk(target), mk(target + 5_000_000), mk(target - 15_000_000)];
        assert_eq!(filter_by_size(results, target).len(), 2);
        // 0 → 空
        assert!(filter_by_size(vec![mk(100)], 0).is_empty());
    }

    #[test]
    fn pieces_hash_extraction() {
        // 构造最小 torrent
        let mut t = Vec::new();
        t.extend_from_slice(b"d4:infod6:lengthi1e4:name1:x6:pieces20:");
        t.extend_from_slice(&[7u8; 20]);
        t.extend_from_slice(b"ee");
        let h = parse_pieces_hash(&t).unwrap();
        // SHA1(0x07 * 20) —— 由 hash 模块 golden 保证正确性
        assert_eq!(h.len(), 40);
    }
}
