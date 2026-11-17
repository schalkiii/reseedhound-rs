//! 辅种引擎：三阶段流水线编排。
//!
//! 对齐 Python src/engine/seeder.py：
//! - 阶段1 扫描：本地 torrent 目录 → 解析 → 去重 → 缓存增量
//! - 阶段2 查询：并发查站点（pieces_hash 通用通道 / HDArea info_hash 通道），
//!   tracker 域名排除 + 历史辅种排除
//! - 阶段3 添加：下载 → 解析校验 → 添加下载器，严重错误/超时熔断站点，
//!   auto_start 语义、凭据错误记录
//!
//! 状态语义：qB pausedUP / stoppedUP（5.x 改名）/ TR completed = 做种完成。
//! 凭据类错误（passkey/Cookie 失效）单独记录，供飞书「更新凭据」提醒。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Mutex, Semaphore};

use sh_core::cache::TorrentCache;
use sh_core::config::{Config, DownloaderRole};
use sh_core::parser::{parse_bytes, scan_directory, TorrentInfo};
use sh_core::stats::ReseedStats;

use crate::downloader::{DownloaderClient, DownloaderConn, DownloaderKind};
use crate::hdarea::{is_hdarea_site, HdareaMatcher};
use crate::jackett::JackettClient;
use crate::site_client::{DeadTorrentCache, DownloadReason, SiteClient, SiteClientConfig};

/// 已停止但内容已校验完成的状态（qB 5.x 改名 stoppedUP；TR 为 completed）。
pub const STOPPED_COMPLETE_STATES: &[&str] = &["pausedUP", "stoppedUP", "completed"];

/// 凭据类错误：passkey / Cookie 失效、权限不足。
pub const CREDENTIAL_ERRORS: &[&str] = &[
    "http_401",
    "http_403",
    "html_permission_denied",
    "html_forbidden",
    "html_auth_required",
    "auth_redirect",
];

/// 严重错误（达到阈值熔断站点）。
pub const SERIOUS_ERRORS: &[&str] = &[
    "http_401",
    "http_403",
    "http_404",
    "http_410",
    "html_permission_denied",
    "html_torrent_deleted",
    "html_not_found",
    "html_forbidden",
    "html_auth_required",
    "auth_redirect",
];

const MAX_SERIOUS_ERRORS: usize = 3;
const MAX_TIMEOUT_ERRORS: usize = 30;

fn reason_str(reason: &DownloadReason) -> String {
    reason.as_str()
}

/// 供报告/飞书通知统计凭据类失败（阶段3 spawn 内无法直接写 stats，
/// 由返回值聚合后统一回填）。
fn is_credential_error(reason: &str) -> bool {
    CREDENTIAL_ERRORS.contains(&reason)
}

fn is_serious_error(reason: &str) -> bool {
    SERIOUS_ERRORS.contains(&reason)
}

/// 引擎运行参数（runner/scheduler 传入）。
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub dry_run: bool,
    /// 站点过滤（逗号分隔，空 = 全部）
    pub site_name: Option<String>,
}

pub struct ReseedEngine {
    config: Arc<Config>,
    cache: Arc<TorrentCache>,
    dead_cache: Arc<DeadTorrentCache>,
    client: Arc<SiteClient>,
    hdarea: Arc<HdareaMatcher>,
    jackett: Option<Arc<JackettClient>>,
    downloader: Mutex<Option<Arc<tokio::sync::Mutex<DownloaderClient>>>>,
    src_downloader: Mutex<Option<Arc<tokio::sync::Mutex<DownloaderClient>>>>,
    mode: String,
    batch_size: usize,
}

impl ReseedEngine {
    pub async fn new(config: Arc<Config>) -> anyhow::Result<Self> {
        let cache = Arc::new(TorrentCache::open(&config.db_path())?);

        // 站点 cookie / host interval 收集
        let mut site_cookies = HashMap::new();
        let mut host_intervals = HashMap::new();
        for s in &config.sites {
            if let Some(cookie) = s.cookie.as_deref().or(s.cookies.as_deref()) {
                if !cookie.is_empty() && !s.url.is_empty() {
                    site_cookies.insert(s.url.clone(), cookie.to_string());
                }
            }
            if let Some(interval) = s.download_interval {
                if interval > 0.0 && !s.url.is_empty() {
                    if let Ok(u) = url::Url::parse(&s.url) {
                        if let Some(host) = u.host_str() {
                            host_intervals.insert(host.to_string(), interval);
                        }
                    }
                }
            }
        }

        let sc = SiteClientConfig {
            concurrency: config.concurrency(),
            api_timeout_secs: config.api_timeout(),
            download_timeout_secs: config.download_timeout(),
            retry_count: config.retry_count(),
            retry_delay_secs: config.retry_delay(),
            download_interval_secs: config.download_interval(),
            host_intervals,
            site_cookies,
        };
        // 死种标记即写 SQLite 持久化（对齐 Python mark_dead → record_dead），
        // 跨轮次/跨重启生效，避免重复下载已知死种。
        let dead_cache = Arc::new(DeadTorrentCache::with_persist({
            let cache = cache.clone();
            move |site, id| {
                if let Err(e) = cache.record_dead(&site, id) {
                    tracing::warn!("死种持久化失败 ({site}, {id}): {e}");
                }
            }
        }));
        let client = Arc::new(SiteClient::new(sc, dead_cache.clone())?);

        let hdarea = HdareaMatcher::new(
            client.clone(),
            config.hdarea_request_interval(),
            config.hdarea_concurrency(),
            config.retry_count(),
        );

        // 模式：jackett 模式需要 jackett.enabled
        let mut mode = config.operation_mode().to_string();
        if (mode == "jackett" || mode == "both") && !config.jackett_enabled() {
            tracing::warn!(
                "运行模式 {mode} 需要 jackett.enabled=true，当前未启用，回退为 pieces_hash"
            );
            mode = "pieces_hash".into();
        }

        let jackett = if mode == "jackett" || mode == "both" {
            Some(Arc::new(JackettClient::new(
                &config.jackett_url(),
                &config.jackett_api_key(),
                config.jackett_search_interval(),
                config.jackett_retry_count().max(config.retry_count()),
                config.retry_delay(),
            )))
        } else {
            None
        };

        Ok(Self {
            config: config.clone(),
            cache,
            dead_cache,
            client,
            hdarea: Arc::new(hdarea),
            jackett,
            downloader: Mutex::new(None),
            src_downloader: Mutex::new(None),
            mode,
            batch_size: config.batch_size(),
        })
    }

    pub fn mode(&self) -> &str {
        &self.mode
    }

    pub async fn start(&self) -> anyhow::Result<()> {
        // 持久化死种灌入
        let persistent_dead = self.cache.load_persistent_dead(15.0)?;
        if !persistent_dead.is_empty() {
            self.dead_cache
                .seed(persistent_dead.into_iter().collect())
                .await;
            tracing::info!(
                "载入持久化死种(>=15天): {} 条，本轮回跳过下载",
                self.dead_cache.size().await
            );
        }
        if let Some(j) = &self.jackett {
            j.load_cache().await;
        }

        // 主下载器 = destination（双下载器模式）或唯一 downloader
        let conn = self.build_conn(DownloaderRole::Destination)?;
        let mut dl = DownloaderClient::new(conn);
        if !self.test_downloader(&mut dl).await {
            anyhow::bail!("下载器连接失败");
        }
        *self.downloader.lock().await = Some(Arc::new(tokio::sync::Mutex::new(dl)));

        if self.config.is_dual_downloader() {
            let src_conn = self.build_conn(DownloaderRole::Source)?;
            let mut src = DownloaderClient::new(src_conn);
            if self.test_downloader(&mut src).await {
                *self.src_downloader.lock().await = Some(Arc::new(tokio::sync::Mutex::new(src)));
                tracing::info!("双下载器模式: 源 -> 目标 连接成功");
            } else {
                tracing::warn!("源下载器连接失败，将使用本地文件路径扫描");
            }
        }
        Ok(())
    }

    fn build_conn(&self, role: DownloaderRole) -> anyhow::Result<DownloaderConn> {
        let conn = self
            .config
            .downloader_conn(role)
            .ok_or(crate::error::EngineError::DownloaderMissingHost)?;
        Ok(DownloaderConn {
            kind: DownloaderKind::from_type(&conn.dl_type)?,
            host: conn.host,
            port: conn.port,
            username: conn.username,
            password: conn.password,
        })
    }

    async fn test_downloader(&self, _dl: &mut DownloaderClient) -> bool {
        // 连接测试：qB 登录 / TR session 探测由首次实际调用完成，这里做轻量探测
        match _dl.conn_kind() {
            DownloaderKind::Qbittorrent => _dl.qb_login().await.is_ok(),
            DownloaderKind::Transmission => _dl.tr_ping().await,
        }
    }

    /// 完整运行（三阶段）。
    pub async fn run(&self, opts: &RunOptions) -> anyhow::Result<ReseedStats> {
        let start = Instant::now();
        let mut stats = ReseedStats {
            start_time: start.elapsed().as_secs_f64(),
            ..Default::default()
        };

        tracing::info!("{}", "=".repeat(60));
        tracing::info!(
            "{} [模式: {}]{}{}",
            sh_core::log_setup::RUN_MARKER,
            self.mode,
            if self.config.is_dual_downloader() {
                " [双下载器]"
            } else {
                ""
            },
            if opts.dry_run {
                " [演练模式] 只查询不添加"
            } else {
                ""
            }
        );
        tracing::info!("{}", "=".repeat(60));

        // 阶段1 扫描
        let torrents = self.phase_scan(&mut stats).await?;

        // 站点过滤
        let mut sites: Vec<_> = self.config.sites.clone();
        if let Some(filter) = &opts.site_name {
            let names: HashSet<String> =
                filter.split(',').map(|s| s.trim().to_lowercase()).collect();
            let found: HashSet<String> = self
                .config
                .sites
                .iter()
                .map(|s| s.name.to_lowercase())
                .collect();
            let missing: Vec<&str> = names
                .iter()
                .filter(|n| !found.contains(*n))
                .map(|n| n.as_str())
                .collect();
            if !missing.is_empty() {
                tracing::warn!("以下站点未在配置中找到: {}", missing.join(", "));
            }
            sites.retain(|s| names.contains(&s.name.to_lowercase()));
            if sites.is_empty() {
                let available: Vec<&str> =
                    self.config.sites.iter().map(|s| s.name.as_str()).collect();
                tracing::error!("未找到指定站点: {filter} (可用: {})", available.join(", "));
                stats.end_time = stats.start_time + start.elapsed().as_secs_f64();
                return Ok(stats);
            }
        }

        let use_pieces_hash = self.mode == "pieces_hash" || self.mode == "both";
        let use_jackett = self.mode == "jackett" || self.mode == "both";

        if use_jackett && !opts.dry_run {
            self.phase_jackett_search(&torrents, &mut stats).await?;
        }

        if use_pieces_hash {
            tracing::info!(
                "启用站点: {} 个, 待查询种子: {} 个 (其中新增 {} 个)",
                sites.len(),
                torrents.len(),
                stats.new_torrents
            );
            let (match_map, download_url_map) = self
                .phase_query_sites(&sites, &torrents, &mut stats)
                .await?;

            if !opts.dry_run {
                self.phase_add_torrents(
                    &match_map,
                    &download_url_map,
                    &sites,
                    &torrents,
                    &mut stats,
                )
                .await?;
            } else {
                tracing::info!("[演练模式] 跳过阶段3（添加种子到下载器）");
            }
        } else if !opts.dry_run {
            tracing::info!("[Jackett模式] 跳过 pieces_hash 查询");
        }

        stats.end_time = stats.start_time + start.elapsed().as_secs_f64();
        stats.dead_count = self.dead_cache.size().await as u64;
        tracing::info!("{}", "=".repeat(60));
        tracing::info!(
            "辅种完成: 扫描={}, 去重={}, 新={}, 匹配={}, 追踪器跳过={}, 成功={}, 失败={}, 死种={}, 耗时={}",
            stats.total_torrents, stats.duplicate_count, stats.new_torrents,
            stats.matched_count, stats.tracker_skip_count, stats.succeeded_count,
            stats.failed_count, stats.dead_count, stats.duration_str()
        );
        tracing::info!("{}", "=".repeat(60));
        Ok(stats)
    }

    /// 阶段1：扫描本地种子。
    async fn phase_scan(&self, stats: &mut ReseedStats) -> anyhow::Result<Vec<TorrentInfo>> {
        tracing::info!("[阶段1] 扫描本地种子文件...");
        let torrent_dir = self.config.downloader_value(
            sh_core::config::DownloaderKey::TorrentDir,
            DownloaderRole::Source,
        );
        if torrent_dir.is_empty() {
            anyhow::bail!("downloader.torrent_dir 未配置：请在 config.yaml 中填写种子文件目录");
        }
        let torrents = scan_directory(&torrent_dir)?;
        stats.total_torrents = torrents.len() as u64;

        // 缓存增量
        let pieces: Vec<String> = torrents.iter().map(|t| t.pieces_hash.clone()).collect();
        let uncached = self.cache.filter_uncached(&pieces)?;
        stats.new_torrents = uncached.len() as u64;
        if !uncached.is_empty() {
            let entries: Vec<(String, String, String)> = torrents
                .iter()
                .filter(|t| uncached.contains(&t.pieces_hash))
                .map(|t| {
                    (
                        t.pieces_hash.clone(),
                        t.info_hash.clone(),
                        t.file_name.clone(),
                    )
                })
                .collect();
            self.cache.add_batch(&entries)?;
        }

        // pieces_hash 去重
        let mut seen = HashSet::new();
        let mut unique = Vec::new();
        for t in torrents {
            if seen.insert(t.pieces_hash.clone()) {
                unique.push(t);
            }
        }
        stats.duplicate_count = stats.total_torrents - unique.len() as u64;

        tracing::info!(
            "扫描完成: 总计={}, 新增={}, 已缓存={}, 去重={}, 待查询={}",
            stats.total_torrents,
            stats.new_torrents,
            stats.total_torrents - stats.new_torrents,
            stats.duplicate_count,
            unique.len()
        );
        Ok(unique)
    }

    /// 阶段2：并发查询站点。
    async fn phase_query_sites(
        &self,
        sites: &[sh_core::config::SiteEntry],
        torrents: &[TorrentInfo],
        stats: &mut ReseedStats,
    ) -> anyhow::Result<(
        HashMap<String, Vec<(String, i64, String)>>,
        HashMap<(String, i64), String>,
    )> {
        tracing::info!("[阶段2] 并发查询各站点...");
        // HDArea 短效直链表：(site_name, torrent_id) -> dl_token 直链
        // （对齐 Python _download_url_map：接口签发 1800 秒有效，阶段3 优先使用）
        let mut download_url_map: HashMap<(String, i64), String> = HashMap::new();

        // info_hash <-> pieces_hash 索引
        let mut info_hash_index: HashMap<String, String> = HashMap::new();
        let mut pieces_to_info: HashMap<&str, &str> = HashMap::new();
        for t in torrents {
            info_hash_index.insert(t.info_hash.to_lowercase(), t.pieces_hash.clone());
            pieces_to_info.insert(t.pieces_hash.as_str(), t.info_hash.as_str());
        }
        // HDArea 短效链接表：由阶段3 按站点配置直接构造 apikey 直链

        // tracker 域名排除 + 历史辅种排除
        let mut domain_to_site: HashMap<String, String> = HashMap::new();
        for s in sites {
            if let Ok(u) = url::Url::parse(&s.url) {
                if let Some(host) = u.host_str() {
                    domain_to_site.insert(host.to_string(), s.name.clone());
                }
            }
        }

        let mut site_exclude: HashMap<String, HashSet<String>> = HashMap::new();
        let mut tracker_total = 0usize;
        for t in torrents {
            if let Some(announce) = &t.announce {
                if let Ok(u) = url::Url::parse(announce) {
                    if let Some(host) = u.host_str() {
                        if let Some(site_name) = domain_to_site.get(host) {
                            site_exclude
                                .entry(site_name.clone())
                                .or_default()
                                .insert(t.pieces_hash.clone());
                            tracker_total += 1;
                        }
                    }
                }
            }
        }
        stats.tracker_skip_count = tracker_total as u64;

        let pieces_all: Vec<String> = torrents.iter().map(|t| t.pieces_hash.clone()).collect();
        let reseeded = self.cache.get_reseeded_hashes_by_site(&pieces_all)?;
        let mut reseed_total = 0usize;
        let qb_pieces: HashSet<String> = torrents.iter().map(|t| t.pieces_hash.clone()).collect();
        for (site_name, hashes) in reseeded {
            let valid: HashSet<String> = hashes
                .into_iter()
                .filter(|h| qb_pieces.contains(h))
                .collect();
            if !valid.is_empty() {
                reseed_total += valid.len();
                site_exclude.entry(site_name).or_default().extend(valid);
            }
        }

        let total_skipped = tracker_total + reseed_total;
        if total_skipped > 0 {
            tracing::info!(
                "总计跳过 {} 个已知种子: {} tracker + {} 历史缓存 ({} 个站点)",
                total_skipped,
                tracker_total,
                reseed_total,
                site_exclude.len()
            );
        }

        // 并发查站（信号量 = global.concurrency）
        let site_sem = Arc::new(Semaphore::new(self.config.concurrency()));
        let mut handles = Vec::new();

        for site in sites {
            let permit = match site_sem.clone().acquire_owned().await {
                Ok(p) => p,
                Err(_) => break,
            };
            let site = site.clone();
            let exclude = site_exclude.get(&site.name).cloned().unwrap_or_default();
            let client = self.client.clone();
            let hdarea = self.hdarea.clone();
            let info_hash_index = info_hash_index.clone();
            let batch_size = self.batch_size;
            let retry_count = self.config.retry_count();
            let site_hashes: Vec<String> = pieces_all
                .iter()
                .filter(|ph| !exclude.contains(ph.as_str()))
                .cloned()
                .collect();
            let pieces_to_info_owned: HashMap<String, String> = pieces_to_info
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();

            handles.push((
                site.name.clone(),
                tokio::spawn(async move {
                    if is_hdarea_site(site.api_type.as_deref().unwrap_or("")) {
                        let info_hashes: Vec<String> = site_hashes
                            .iter()
                            .filter_map(|ph| pieces_to_info_owned.get(ph))
                            .cloned()
                            .collect();
                        tracing::info!(
                            "查询站点: {} (HDArea info_hash 通道, {} 个种子)",
                            site.name,
                            info_hashes.len()
                        );
                        let matches = hdarea
                            .query(
                                &site.name,
                                site.api_url.as_deref().unwrap_or(""),
                                site.url.as_str(),
                                site.api_key.as_deref().unwrap_or(""),
                                &info_hashes,
                            )
                            .await;
                        let mut results = Vec::new();
                        let mut site_dl_urls = Vec::new();
                        for m in matches {
                            let Some(pieces_hash) = info_hash_index.get(&m.info_hash) else {
                                continue;
                            };
                            if !m.download_url.is_empty() {
                                site_dl_urls
                                    .push(((site.name.clone(), m.torrent_id), m.download_url));
                            }
                            results.push((pieces_hash.clone(), m.torrent_id));
                        }
                        (site, results, site_dl_urls)
                    } else {
                        tracing::info!("查询站点: {} ({} 个种子)", site.name, site_hashes.len());
                        let mut results = Vec::new();
                        for batch in site_hashes.chunks(batch_size.max(1)) {
                            let body = serde_json::json!({
                                "passkey": site.passkey,
                                "pieces_hash": batch,
                            });
                            let Some(api_url) = site.api_url.clone() else {
                                continue;
                            };
                            let resp = client
                                .post_json(&api_url, &body, None, retry_count.max(1), "")
                                .await;
                            if let Some(v) = resp {
                                if let Some(data) = v.get("data").and_then(|d| d.as_object()) {
                                    for (ph, tid) in data {
                                        if ph == "passkey" || ph == "pieces_hash" {
                                            continue;
                                        }
                                        let id = match tid {
                                            serde_json::Value::Number(n) => n.as_i64().unwrap_or(0),
                                            serde_json::Value::String(sv) => {
                                                sv.parse::<i64>().unwrap_or(0)
                                            }
                                            _ => 0,
                                        };
                                        if id > 0 {
                                            results.push((ph.clone(), id));
                                        }
                                    }
                                }
                            }
                        }
                        (site, results, Vec::new())
                    }
                }),
            ));
            drop(permit);
        }

        // 收集
        let mut match_map: HashMap<String, Vec<(String, i64, String)>> = HashMap::new();
        let mut site_match_counts: HashMap<String, u64> = HashMap::new();
        for (site_name, handle) in handles {
            let (site, results, site_dl_urls) = match handle.await {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("站点 {site_name} 查询异常: {e}");
                    stats.failed_sites.push(site_name.clone());
                    continue;
                }
            };
            // 记录 HDArea 短效直链（覆盖同键以先到者为准，与 Python 一致）
            for (key, url) in site_dl_urls {
                download_url_map.entry(key).or_insert(url);
            }
            stats
                .site_details
                .insert(site_name.clone(), results.len() as u64);
            tracing::info!("站点 {} 匹配到 {} 个种子", site.name, results.len());
            if results.is_empty() {
                stats.failed_sites.push(site_name.clone());
            }
            for (pieces_hash, torrent_id) in results {
                match_map.entry(pieces_hash.clone()).or_default().push((
                    site.name.clone(),
                    torrent_id,
                    site.url.clone(),
                ));
                *site_match_counts.entry(site.name.clone()).or_insert(0) += 1;
            }
        }

        stats.matched_count = match_map.len() as u64;
        tracing::info!("全局匹配: {} 个种子可跨站辅种", stats.matched_count);
        stats.site_match_counts = site_match_counts;

        Ok((match_map, download_url_map))
    }

    /// 阶段3：添加种子到下载器。
    #[allow(clippy::too_many_arguments)]
    async fn phase_add_torrents(
        &self,
        match_map: &HashMap<String, Vec<(String, i64, String)>>,
        download_url_map: &HashMap<(String, i64), String>,
        sites: &[sh_core::config::SiteEntry],
        torrents: &[TorrentInfo],
        stats: &mut ReseedStats,
    ) -> anyhow::Result<()> {
        tracing::info!("[阶段3] 添加种子到下载器...");
        if match_map.is_empty() {
            tracing::info!("没有可辅种的种子");
            return Ok(());
        }

        let add_sem = Arc::new(Semaphore::new(20));
        let no_access_sites = Arc::new(Mutex::new(HashSet::new()));
        let site_serious_errors = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
        let site_timeout_errors = Arc::new(Mutex::new(HashMap::<String, usize>::new()));

        // 下载器现有种子 info_hash 全量集合（增量去重 + 统计口径对齐 Python：
        // 已在下载器的种子直接记成功，不再发起 add 请求）
        let qb_existing_hashes: HashSet<String> = {
            let guard = self.downloader.lock().await;
            match &*guard {
                Some(d) => d.lock().await.qb_get_all_info_hashes().await.unwrap_or_default(),
                None => HashSet::new(),
            }
        };

        let site_configs: HashMap<&str, &sh_core::config::SiteEntry> =
            sites.iter().map(|s| (s.name.as_str(), s)).collect();

        let skip_hash = self.config.downloader_flag(
            sh_core::config::DownloaderFlag::SkipHashCheck,
            DownloaderRole::Destination,
        );
        let auto_start = self.config.downloader_flag(
            sh_core::config::DownloaderFlag::AutoStart,
            DownloaderRole::Destination,
        );
        let torrent_tag = self.config.downloader_value(
            sh_core::config::DownloaderKey::Tag,
            DownloaderRole::Destination,
        );

        let pieces_index: HashMap<&str, &TorrentInfo> = torrents
            .iter()
            .map(|t| (t.pieces_hash.as_str(), t))
            .collect();

        let mut handles = Vec::new();
        for (pieces_hash, matches) in match_map {
            let permit = match add_sem.clone().acquire_owned().await {
                Ok(p) => p,
                Err(_) => break,
            };
            // 克隆需要的数据（Spawn 生命周期）
            let entry = match pieces_index.get(pieces_hash.as_str()) {
                Some(e) => (*e).clone(),
                None => {
                    drop(permit);
                    continue;
                }
            };
            let matches = matches.clone();
            // 本种子命中的短效直链子集（spawn 生命周期，需克隆）
            let dl_urls_owned: HashMap<(String, i64), String> = matches
                .iter()
                .filter_map(|(sn, tid, _)| {
                    download_url_map
                        .get(&(sn.clone(), *tid))
                        .map(|u| ((sn.clone(), *tid), u.clone()))
                })
                .collect();
            let site_configs_owned: Vec<sh_core::config::SiteEntry> = matches
                .iter()
                .map(|(sn, _, _)| site_configs.get(sn.as_str()).map(|s| (*s).clone()))
                .collect::<Option<Vec<_>>>()
                .unwrap_or_default();
            let torrent_tag = torrent_tag.clone();
            let no_access = no_access_sites.clone();
            let serious = site_serious_errors.clone();
            let timeouts = site_timeout_errors.clone();
            let qb_existing = qb_existing_hashes.clone();
            let client = self.client.clone();
            let cache = self.cache.clone();
            let dl = self.downloader.lock().await.clone(); // Option<Arc<Mutex<DownloaderClient>>>

            handles.push(tokio::spawn(async move {
                let mut piece_failed = 0u64;
                let mut piece_failed_by_site: HashMap<String, u64> = HashMap::new();
                let mut credential_failures: HashMap<String, String> = HashMap::new();
                let mut skipped = false;
                let mut source_save_path: Option<String> = None;

                // 源种子状态前置检查（对齐 Python get_torrent_info）：
                // 不在下载器 / 正在下载中则整颗跳过，避免无效的站点下载请求；
                // 同时取源种子 save_path，供辅种种子落盘到同一目录。
                if let Some(dl) = dl.as_ref() {
                    let guard = dl.lock().await;
                    let info = guard.qb_torrent_info(&entry.info_hash).await;
                    drop(guard);
                    match info.as_ref().map(|(_, state)| state.as_str()) {
                        None => {
                            tracing::debug!(
                                "跳过辅种: 源种子 {} 无法从下载器获取信息(可能已移除/离线)",
                                entry.file_name
                            );
                            skipped = true;
                        }
                        Some("downloading") => {
                            tracing::debug!("跳过辅种: 源种子 {} 正在下载中", entry.file_name);
                            skipped = true;
                        }
                        Some(_) => {
                            source_save_path = info.map(|(path, _)| path).filter(|p| !p.is_empty());
                        }
                    }
                }

                for (i, (site_name, torrent_id, site_url)) in matches.iter().enumerate() {
                    if skipped {
                        break;
                    }
                    let site_cfg = match site_configs_owned.get(i) {
                        Some(s) => s,
                        None => continue,
                    };
                    {
                        let na = no_access.lock().await;
                        if na.contains(site_name) {
                            continue;
                        }
                    }

                    // HDArea 通道优先接口签发的 dl_token 短效直链（1800 秒有效），
                    // 无短效链接再退回 apikey/passkey 拼接（对齐 Python 行为）。
                    let dl_url = if let Some(u) = dl_urls_owned.get(&(site_name.clone(), *torrent_id)) {
                        u.clone()
                    } else {
                        match site_cfg.api_type.as_deref().unwrap_or("") {
                            t if is_hdarea_site(t) => {
                                tracing::warn!(
                                    "站点 {site_name} 无短效下载链接，退回 apikey 直链 (id={torrent_id})"
                                );
                                format!("{site_url}/download.php?id={torrent_id}&apikey={}", site_cfg.api_key.as_deref().unwrap_or(""))
                            }
                            _ => {
                                let passkey = site_cfg.passkey.as_deref().unwrap_or("");
                                if passkey.is_empty() {
                                    tracing::warn!(
                                        "站点 {site_name} 既无短效下载链接也无 passkey，跳过 (id={torrent_id})"
                                    );
                                    continue;
                                }
                                format!("{site_url}/download.php?id={torrent_id}&passkey={passkey}")
                            }
                        }
                    };

                    let (data, reason) = client
                        .try_download(&dl_url, site_name, *torrent_id)
                        .await;
                    let Some(torrent_data) = data else {
                        let reason_s = reason_str(&reason);
                        if reason_s == "dead_cached" {
                            // 已知死种：静默跳过
                            continue;
                        }
                        piece_failed += 1;
                        *piece_failed_by_site.entry(site_name.clone()).or_insert(0) += 1;
                        if reason_s == "timeout" {
                            let mut t = timeouts.lock().await;
                            let c = t.entry(site_name.clone()).or_insert(0);
                            *c += 1;
                            if *c >= MAX_TIMEOUT_ERRORS {
                                let mut na = no_access.lock().await;
                                if na.insert(site_name.clone()) {
                                    tracing::warn!(
                                        "站点 {site_name} 累计 {c} 次下载超时，跳过后续下载"
                                    );
                                }
                            }
                        } else if is_serious_error(&reason_s) {
                            // 凭据类错误记录首个原因，供飞书「去更新凭据」提醒
                            // （对齐 Python setdefault 语义：同站点只留首个）
                            if is_credential_error(&reason_s) {
                                credential_failures
                                    .entry(site_name.clone())
                                    .or_insert_with(|| reason_s.clone());
                            }
                            let mut s = serious.lock().await;
                            let c = s.entry(site_name.clone()).or_insert(0);
                            *c += 1;
                            if *c >= MAX_SERIOUS_ERRORS {
                                let mut na = no_access.lock().await;
                                if na.insert(site_name.clone()) {
                                    tracing::warn!(
                                        "站点 {site_name} ({c}次严重错误: {reason_s})，跳过后续下载"
                                    );
                                }
                            }
                        }
                        continue;
                    };

                    let parsed = parse_bytes(&torrent_data, &entry.file_name, &entry.file_path);
                    let Some(parsed) = parsed else {
                        piece_failed += 1;
                        *piece_failed_by_site.entry(site_name.clone()).or_insert(0) += 1;
                        tracing::warn!(
                            "下载的不是有效torrent文件: {} (站点: {site_name}, 大小: {})",
                            entry.file_name, torrent_data.len()
                        );
                        continue;
                    };

                    // 存活确认：清除死种记录
                    let _ = cache.clear_dead_if_alive(site_name, *torrent_id);

                    // 增量去重（对齐 Python qb_info_hashes）：已存在直接记成功
                    // 并写入辅种记录，跳过下载器 add 请求（双方均为小写 hex）
                    if qb_existing.contains(&parsed.info_hash) {
                        let _ = cache.add_reseed_record(&entry.pieces_hash, site_name, *torrent_id);
                        tracing::debug!(
                            "已在下载器: {} -> {site_name} (id={torrent_id})，记成功",
                            entry.file_name
                        );
                        continue;
                    }

                    // 添加（save_path 传源种子保存目录，辅种种子落盘同目录）
                    let Some(dl) = dl.clone() else { continue };
                    let mut dl = dl.lock().await;
                    let result = dl
                        .add_torrent_bytes(
                            &torrent_data,
                            skip_hash,
                            auto_start,
                            Some(torrent_tag.as_str()),
                            &parsed.info_hash,
                            source_save_path.as_deref(),
                        )
                        .await;
                    if result {
                        let _ = cache.add_reseed_record(&entry.pieces_hash, site_name, *torrent_id);
                        tracing::info!(
                            "辅种成功: {} -> {site_name} (id={torrent_id})",
                            entry.file_name
                        );
                    } else {
                        piece_failed += 1;
                        *piece_failed_by_site.entry(site_name.clone()).or_insert(0) += 1;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs_f64(0.3)).await;
                }

                (piece_failed, piece_failed_by_site, credential_failures, skipped)
            }));
            drop(permit);
        }

        for h in handles {
            if let Ok((failed, by_site, creds, skipped)) = h.await {
                if skipped {
                    stats.skipped_count += 1;
                }
                stats.failed_count += failed;
                for (site, cnt) in by_site {
                    *stats.site_failed.entry(site).or_insert(0) += cnt;
                }
                // setdefault 语义：同站点只保留最先收到的凭据错误原因
                for (site, reason) in creds {
                    stats
                        .credential_failures
                        .entry(site)
                        .or_insert(reason);
                }
            }
        }
        Ok(())
    }

    /// Jackett 阶段（对齐 Python _phase_jackett_search 的核心语义）。
    async fn phase_jackett_search(
        &self,
        torrents: &[TorrentInfo],
        stats: &mut ReseedStats,
    ) -> anyhow::Result<()> {
        let Some(jackett) = &self.jackett else {
            return Ok(());
        };
        tracing::info!("[Jackett] 通过 Jackett 搜索辅种...");
        let dl_guard = self.downloader.lock().await;
        let qb_hashes = match &*dl_guard {
            Some(d) => d
                .lock()
                .await
                .qb_get_all_info_hashes()
                .await
                .unwrap_or_default(),
            None => HashSet::new(),
        };

        let tag = self.config.downloader_value(
            sh_core::config::DownloaderKey::Tag,
            DownloaderRole::Destination,
        );

        for (idx, t) in torrents.iter().enumerate() {
            let mut search_name = t
                .torrent_name
                .clone()
                .unwrap_or_else(|| t.file_name.clone());
            if let Some(stripped) = search_name.strip_suffix(".torrent") {
                search_name = stripped.to_string();
            }
            let torrent_size = t.total_size.unwrap_or(0);
            if search_name.is_empty() || torrent_size == 0 {
                continue;
            }
            if (idx + 1) % 10 == 0 || idx == 0 {
                let jstats = jackett.stats.lock().await;
                tracing::info!(
                    "Jackett 进度 [{}/{}], 下载={}, 成功={}",
                    idx + 1,
                    torrents.len(),
                    jstats.downloaded_count,
                    stats.succeeded_count
                );
            }

            let downloaded = jackett
                .search_and_download(&search_name, torrent_size, &t.pieces_hash, &self.client)
                .await;
            let Some((torrent_data, result)) = downloaded else {
                continue;
            };
            let parsed = parse_bytes(&torrent_data, &search_name, "");
            let Some(parsed) = parsed else {
                stats.failed_count += 1;
                continue;
            };
            if qb_hashes.contains(&parsed.info_hash) {
                continue;
            }
            let indexer = if result.indexer_name.is_empty() {
                "jackett".to_string()
            } else {
                result.indexer_name.clone()
            };
            // 源种子 save_path（对齐 Python Jackett 分支 get_torrent_info）
            let dl_guard = self.downloader.lock().await;
            if let Some(d) = &*dl_guard {
                let mut d = d.lock().await;
                let source_save_path = d
                    .qb_torrent_info(&t.info_hash)
                    .await
                    .map(|(path, _)| path)
                    .filter(|p| !p.is_empty());
                let ok = d
                    .add_torrent_bytes(
                        &torrent_data,
                        false,
                        false,
                        Some(tag.as_str()),
                        &parsed.info_hash,
                        source_save_path.as_deref(),
                    )
                    .await;
                if ok {
                    stats.succeeded_count += 1;
                    *stats.site_succeeded.entry(indexer.clone()).or_insert(0) += 1;
                    *stats.site_details.entry(indexer.clone()).or_insert(0) += 1;
                    stats.matched_count += 1;
                } else {
                    stats.failed_count += 1;
                }
            }
        }
        Ok(())
    }
}
