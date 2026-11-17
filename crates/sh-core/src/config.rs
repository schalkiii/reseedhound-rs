//! 配置加载与默认值单一来源（对齐 Python src/storage/config.py）。
//!
//! 行为对齐要点：
//! - DEFAULTS 是所有可缺省字段的唯一权威定义
//! - 未知字段只告警不阻断（两遍解析：serde_yaml::Value 预扫描 → 强类型）
//! - 站点准入：passkey 或 api_key 至少一项，否则静默跳过
//! - enable 优先于 enabled（历史兼容）；cookie 优先于 cookies
//! - downloader 双角色：source/destination 子块存在时各自独立，否则同一块

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

// ---------- 默认值（与 Python DEFAULTS 逐字对齐） ----------

pub mod defaults {
    pub const GLOBAL_CONCURRENCY: usize = 20;
    pub const GLOBAL_BATCH_SIZE: usize = 100;
    pub const GLOBAL_API_TIMEOUT: f64 = 60.0;
    pub const GLOBAL_DOWNLOAD_TIMEOUT: f64 = 15.0;
    pub const GLOBAL_DOWNLOAD_INTERVAL: f64 = 5.0;
    pub const GLOBAL_RETRY_COUNT: u32 = 2;
    pub const GLOBAL_RETRY_DELAY: f64 = 2.0;
    pub const GLOBAL_MODE: &str = "pieces_hash";
    pub const DB_PATH: &str = "data/cache.db";
    pub const LOG_LEVEL: &str = "INFO";
    pub const LOG_FILE: &str = "logs/reseed.log";
    pub const SCHEDULER_ENABLED: bool = false;
    pub const SCHEDULER_INTERVAL_MINUTES: f64 = 360.0;
    pub const SCHEDULER_STARTUP_DELAY: f64 = 0.0;
    pub const SCHEDULER_RUN_ON_START: bool = true;
    pub const DL_TORRENT_DIR: &str = "";
    pub const DL_SKIP_HASH_CHECK: bool = true;
    pub const DL_AUTO_START: bool = true;
    pub const DL_TAG: &str = "SeedHound";
    pub const JACKETT_ENABLED: bool = false;
    pub const JACKETT_URL: &str = "http://localhost:9117";
    pub const JACKETT_API_KEY: &str = "";
    pub const JACKETT_CONCURRENCY: usize = 5;
    pub const JACKETT_SEARCH_INTERVAL: f64 = 1.0;
    pub const JACKETT_TIMEOUT: f64 = 30.0;
    pub const CC_ENABLED: bool = false;
    pub const CC_URL: &str = "http://127.0.0.1:8082/cookie";
    pub const HDAREA_REQUEST_INTERVAL: f64 = 1.1;
    pub const HDAREA_CONCURRENCY: usize = 2;
    pub const SAVEPT_URL: &str = "https://savept.icu";
    pub const SAVEPT_TIMEOUT: f64 = 20.0;
    pub const SITES_ENABLED: bool = true;
}

// ---------- 强类型配置结构 ----------

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct GlobalSection {
    pub concurrency: Option<usize>,
    pub batch_size: Option<usize>,
    pub api_timeout: Option<f64>,
    pub download_timeout: Option<f64>,
    pub download_interval: Option<f64>,
    pub retry_count: Option<u32>,
    pub retry_delay: Option<f64>,
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct DbSection {
    pub path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct LogSection {
    pub level: Option<String>,
    pub file: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct SchedulerSection {
    pub enabled: Option<bool>,
    pub interval_minutes: Option<f64>,
    pub startup_delay: Option<f64>,
    pub run_on_start: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct DownloaderSection {
    #[serde(rename = "type")]
    pub dl_type: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub torrent_dir: Option<String>,
    pub skip_hash_check: Option<bool>,
    pub auto_start: Option<bool>,
    pub tag: Option<String>,
    pub source: Option<Box<DownloaderSection>>,
    pub destination: Option<Box<DownloaderSection>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct FeishuSection {
    pub webhook_url: Option<String>,
    pub secret: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct NotifySection {
    pub feishu: Option<FeishuSection>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct JackettSection {
    pub enabled: Option<bool>,
    pub url: Option<String>,
    pub api_key: Option<String>,
    pub concurrency: Option<usize>,
    pub search_interval: Option<f64>,
    pub timeout: Option<f64>,
    pub retry_count: Option<u32>,
    pub retry_delay: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct CookieCloudSection {
    pub enabled: Option<bool>,
    /// 历史拼写，优先级高于 enabled（对齐 Python 行为）
    pub enable: Option<bool>,
    pub url: Option<String>,
    pub uuid: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct HdareaSection {
    pub request_interval: Option<f64>,
    pub concurrency: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct SaveptSection {
    pub url: Option<String>,
    pub timeout: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ConfigFile {
    pub global: Option<GlobalSection>,
    pub db: Option<DbSection>,
    pub log: Option<LogSection>,
    pub scheduler: Option<SchedulerSection>,
    pub downloader: Option<DownloaderSection>,
    pub notify: Option<NotifySection>,
    pub jackett: Option<JackettSection>,
    pub cookiecloud: Option<CookieCloudSection>,
    pub hdarea: Option<HdareaSection>,
    pub savept: Option<SaveptSection>,
}

// ---------- 站点配置 ----------

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct SiteEntry {
    pub name: String,
    pub url: String,
    pub api_url: Option<String>,
    pub passkey: Option<String>,
    pub api_type: Option<String>,
    pub api_key: Option<String>,
    /// 历史拼写，优先级高于 enabled
    pub enable: Option<bool>,
    pub enabled: Option<bool>,
    pub cookie: Option<String>,
    pub cookies: Option<String>,
    pub download_interval: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct SitesFile {
    pub sites: Vec<SiteEntry>,
}

// ---------- 已知字段白名单（未知字段告警） ----------

fn known_keys(section: &str) -> Option<&'static [&'static str]> {
    Some(match section {
        "global" => &["concurrency", "batch_size", "api_timeout", "download_timeout",
            "download_interval", "retry_count", "retry_delay", "mode"],
        "db" => &["path"],
        "log" => &["level", "file"],
        "scheduler" => &["enabled", "interval_minutes", "startup_delay", "run_on_start"],
        "downloader" => &["type", "host", "port", "username", "password", "torrent_dir",
            "skip_hash_check", "auto_start", "tag", "source", "destination"],
        "jackett" => &["enabled", "url", "api_key", "concurrency", "search_interval",
            "timeout", "retry_count", "retry_delay"],
        "cookiecloud" => &["enabled", "enable", "url", "uuid", "password"],
        "hdarea" => &["request_interval", "concurrency"],
        "savept" => &["url", "timeout"],
        "notify" => &["feishu"],
        "feishu" => &["webhook_url", "secret"],
        _ => return None,
    })
}

const KNOWN_SITE_KEYS: &[&str] = &[
    "name", "url", "api_url", "passkey", "api_type", "api_key",
    "enabled", "enable", "cookie", "cookies", "download_interval",
];

fn collect_unknown_fields(value: &serde_yaml::Value, unknown: &mut BTreeSet<String>) {
    let Some(mapping) = value.as_mapping() else {
        return;
    };
    for (k, v) in mapping {
        let Some(section) = k.as_str() else { continue };
        let Some(allowed) = known_keys(section) else {
            unknown.insert(format!("配置块 {section}"));
            continue;
        };
        if let Some(body) = v.as_mapping() {
            for (fk, fv) in body {
                let Some(field) = fk.as_str() else { continue };
                if !allowed.contains(&field) {
                    unknown.insert(format!("{section}.{field}"));
                }
                // 嵌套校验：downloader.source/destination 与 notify.feishu
                if section == "downloader" && matches!(field, "source" | "destination") {
                    if let Some(sub) = fv.as_mapping() {
                        for (sk, _) in sub {
                            if let Some(s) = sk.as_str() {
                                if !allowed.contains(&s) {
                                    unknown.insert(format!("downloader.{field}.{s}"));
                                }
                            }
                        }
                    }
                }
                if section == "notify" && field == "feishu" {
                    if let Some(sub) = fv.as_mapping() {
                        for (sk, _) in sub {
                            if let Some(s) = sk.as_str() {
                                if !known_keys("feishu").unwrap().contains(&s) {
                                    unknown.insert(format!("notify.feishu.{s}"));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

// ---------- 统一配置视图 ----------

#[derive(Debug, Clone)]
pub struct Config {
    file: ConfigFile,
    pub sites_path: PathBuf,
    /// 准入过滤后的启用站点
    pub sites: Vec<SiteEntry>,
}

impl Config {
    pub fn load(config_path: &str, sites_path: &str) -> anyhow::Result<Self> {
        let raw = load_yaml(Path::new(config_path))?;
        let raw_sites = load_yaml(Path::new(sites_path))?;

        // 未知字段告警（不阻断）
        let mut unknown = BTreeSet::new();
        collect_unknown_fields(&raw, &mut unknown);
        if let Some(sites_val) = raw_sites.get("sites") {
            if let Some(list) = sites_val.as_sequence() {
                for site in list {
                    if let Some(mapping) = site.as_mapping() {
                        for (k, _) in mapping {
                            if let Some(key) = k.as_str() {
                                if !KNOWN_SITE_KEYS.contains(&key) {
                                    unknown.insert(format!("sites[].{key}"));
                                }
                            }
                        }
                    }
                }
            }
        }
        if !unknown.is_empty() {
            let joined: Vec<String> = unknown.into_iter().collect();
            tracing::warn!(
                "配置中发现未识别字段（将被忽略，请检查拼写）: {}",
                joined.join(", ")
            );
        }

        let file: ConfigFile = serde_yaml::from_value(raw)?;
        let sites_file: SitesFile = serde_yaml::from_value(raw_sites)?;
        let sites = filter_enabled_sites(sites_file.sites);

        Ok(Self {
            file,
            sites_path: PathBuf::from(sites_path),
            sites,
        })
    }

    /// 重新加载 sites.yaml（CookieCloud 同步写回后调用，使本轮引擎使用最新 Cookie）。
    pub fn reload_sites(&mut self) -> anyhow::Result<()> {
        let raw_sites = load_yaml(&self.sites_path)?;
        let sites_file: SitesFile = serde_yaml::from_value(raw_sites)?;
        self.sites = filter_enabled_sites(sites_file.sites);
        Ok(())
    }

    fn global(&self) -> &GlobalSection {
        self.file.global.as_ref().unwrap_or(&EMPTY_GLOBAL)
    }

    pub fn concurrency(&self) -> usize {
        self.global().concurrency.unwrap_or(defaults::GLOBAL_CONCURRENCY)
    }
    pub fn batch_size(&self) -> usize {
        self.global().batch_size.unwrap_or(defaults::GLOBAL_BATCH_SIZE)
    }
    pub fn api_timeout(&self) -> f64 {
        self.global().api_timeout.unwrap_or(defaults::GLOBAL_API_TIMEOUT)
    }
    pub fn download_timeout(&self) -> f64 {
        self.global().download_timeout.unwrap_or(defaults::GLOBAL_DOWNLOAD_TIMEOUT)
    }
    pub fn download_interval(&self) -> f64 {
        self.global().download_interval.unwrap_or(defaults::GLOBAL_DOWNLOAD_INTERVAL)
    }
    pub fn retry_count(&self) -> u32 {
        self.global().retry_count.unwrap_or(defaults::GLOBAL_RETRY_COUNT)
    }
    pub fn retry_delay(&self) -> f64 {
        self.global().retry_delay.unwrap_or(defaults::GLOBAL_RETRY_DELAY)
    }
    pub fn operation_mode(&self) -> String {
        self.global()
            .mode
            .clone()
            .unwrap_or_else(|| defaults::GLOBAL_MODE.to_string())
    }
    /// CLI --mode 临时覆盖（不写回文件，对齐 Python build_config 的 setdefault 注入）
    pub fn override_mode(&mut self, mode: &str) {
        let g = self.file.global.get_or_insert_with(Default::default);
        g.mode = Some(mode.to_string());
    }

    pub fn db_path(&self) -> String {
        self.file
            .db
            .as_ref()
            .and_then(|d| d.path.clone())
            .unwrap_or_else(|| defaults::DB_PATH.to_string())
    }

    pub fn log_level(&self) -> String {
        self.file
            .log
            .as_ref()
            .and_then(|l| l.level.clone())
            .unwrap_or_else(|| defaults::LOG_LEVEL.to_string())
    }
    pub fn log_file(&self) -> String {
        self.file
            .log
            .as_ref()
            .and_then(|l| l.file.clone())
            .unwrap_or_else(|| defaults::LOG_FILE.to_string())
    }

    // scheduler
    pub fn scheduler_enabled(&self) -> bool {
        self.file
            .scheduler
            .as_ref()
            .and_then(|s| s.enabled)
            .unwrap_or(defaults::SCHEDULER_ENABLED)
    }
    pub fn scheduler_interval_seconds(&self) -> f64 {
        let minutes = self
            .file
            .scheduler
            .as_ref()
            .and_then(|s| s.interval_minutes)
            .unwrap_or(defaults::SCHEDULER_INTERVAL_MINUTES);
        (minutes * 60.0).max(1.0)
    }
    pub fn scheduler_startup_delay(&self) -> f64 {
        self.file
            .scheduler
            .as_ref()
            .and_then(|s| s.startup_delay)
            .unwrap_or(defaults::SCHEDULER_STARTUP_DELAY)
            .max(0.0)
    }
    pub fn scheduler_run_on_start(&self) -> bool {
        self.file
            .scheduler
            .as_ref()
            .and_then(|s| s.run_on_start)
            .unwrap_or(defaults::SCHEDULER_RUN_ON_START)
    }

    // downloader：单/双下载器统一入口
    fn dl_section(&self, role: DownloaderRole) -> &DownloaderSection {
        let root = self.file.downloader.as_ref().unwrap_or(&EMPTY_DOWNLOADER);
        let sub = match role {
            DownloaderRole::Source => root.source.as_deref(),
            DownloaderRole::Destination => root.destination.as_deref(),
        };
        sub.unwrap_or(root)
    }

    pub fn is_dual_downloader(&self) -> bool {
        let Some(root) = self.file.downloader.as_ref() else {
            return false;
        };
        root.source.is_some() && root.destination.is_some()
    }

    pub fn downloader_value(&self, key: DownloaderKey, role: DownloaderRole) -> String {
        match key {
            DownloaderKey::TorrentDir => self
                .dl_section(role)
                .torrent_dir
                .clone()
                .unwrap_or_else(|| defaults::DL_TORRENT_DIR.to_string()),
            DownloaderKey::Tag => self
                .dl_section(role)
                .tag
                .clone()
                .unwrap_or_else(|| defaults::DL_TAG.to_string()),
        }
    }

    pub fn downloader_flag(&self, key: DownloaderFlag, role: DownloaderRole) -> bool {
        let section = self.dl_section(role);
        match key {
            DownloaderFlag::SkipHashCheck => section
                .skip_hash_check
                .unwrap_or(defaults::DL_SKIP_HASH_CHECK),
            DownloaderFlag::AutoStart => section.auto_start.unwrap_or(defaults::DL_AUTO_START),
        }
    }

    /// 连接信息（必填项缺省时 None；调用方负责报错）
    pub fn downloader_conn(&self, role: DownloaderRole) -> Option<DownloaderConn> {
        let section = self.dl_section(role);
        Some(DownloaderConn {
            dl_type: section
                .dl_type
                .clone()
                .unwrap_or_else(|| "qbittorrent".to_string()),
            host: section.host.clone()?,
            port: section.port.unwrap_or(8080),
            username: section.username.clone().unwrap_or_else(|| "admin".to_string()),
            password: section.password.clone().unwrap_or_default(),
        })
    }

    // feishu

    pub fn feishu_webhook_url(&self) -> String {
        self.file
            .notify
            .as_ref()
            .and_then(|n| n.feishu.as_ref())
            .and_then(|f| f.webhook_url.clone())
            .unwrap_or_default()
    }

    pub fn feishu_secret(&self) -> Option<String> {
        self.file
            .notify
            .as_ref()
            .and_then(|n| n.feishu.as_ref())
            .and_then(|f| f.secret.clone())
    }

    // jackett
    pub fn jackett_enabled(&self) -> bool {
        let j = self.file.jackett.as_ref();
        j.and_then(|s| s.enabled)
            .unwrap_or(defaults::JACKETT_ENABLED)
            && !self.jackett_api_key().is_empty()
    }
    pub fn jackett_url(&self) -> String {
        self.file
            .jackett
            .as_ref()
            .and_then(|j| j.url.clone())
            .unwrap_or_else(|| defaults::JACKETT_URL.to_string())
    }
    pub fn jackett_api_key(&self) -> String {
        self.file
            .jackett
            .as_ref()
            .and_then(|j| j.api_key.clone())
            .unwrap_or_else(|| defaults::JACKETT_API_KEY.to_string())
    }
    pub fn jackett_timeout(&self) -> f64 {
        self.file
            .jackett
            .as_ref()
            .and_then(|j| j.timeout)
            .unwrap_or(defaults::JACKETT_TIMEOUT)
    }
    pub fn jackett_concurrency(&self) -> usize {
        self.file
            .jackett
            .as_ref()
            .and_then(|j| j.concurrency)
            .unwrap_or(defaults::JACKETT_CONCURRENCY)
    }
    pub fn jackett_search_interval(&self) -> f64 {
        self.file
            .jackett
            .as_ref()
            .and_then(|j| j.search_interval)
            .unwrap_or(defaults::JACKETT_SEARCH_INTERVAL)
    }
    pub fn jackett_retry_count(&self) -> u32 {
        self.file
            .jackett
            .as_ref()
            .and_then(|j| j.retry_count)
            .unwrap_or_else(|| self.retry_count())
    }
    pub fn jackett_retry_delay(&self) -> f64 {
        self.file
            .jackett
            .as_ref()
            .and_then(|j| j.retry_delay)
            .unwrap_or_else(|| self.retry_delay())
    }

    // cookiecloud（enable 优先，历史兼容）
    pub fn cookiecloud_enabled(&self) -> bool {
        let cc = self.file.cookiecloud.as_ref();
        if let Some(enable) = cc.and_then(|c| c.enable) {
            return enable;
        }
        cc.and_then(|c| c.enabled)
            .unwrap_or(defaults::CC_ENABLED)
    }
    pub fn cookiecloud_url(&self) -> String {
        self.file
            .cookiecloud
            .as_ref()
            .and_then(|c| c.url.clone())
            .unwrap_or_else(|| defaults::CC_URL.to_string())
    }
    pub fn cookiecloud_uuid(&self) -> Option<String> {
        self.file.cookiecloud.as_ref().and_then(|c| c.uuid.clone())
    }
    pub fn cookiecloud_password(&self) -> Option<String> {
        self.file
            .cookiecloud
            .as_ref()
            .and_then(|c| c.password.clone())
    }

    // hdarea
    pub fn hdarea_request_interval(&self) -> f64 {
        self.file
            .hdarea
            .as_ref()
            .and_then(|h| h.request_interval)
            .unwrap_or(defaults::HDAREA_REQUEST_INTERVAL)
    }
    pub fn hdarea_concurrency(&self) -> usize {
        self.file
            .hdarea
            .as_ref()
            .and_then(|h| h.concurrency)
            .unwrap_or(defaults::HDAREA_CONCURRENCY)
    }

    // savept
    pub fn savept_url(&self) -> String {
        self.file
            .savept
            .as_ref()
            .and_then(|s| s.url.clone())
            .unwrap_or_else(|| defaults::SAVEPT_URL.to_string())
    }
    pub fn savept_timeout(&self) -> f64 {
        self.file
            .savept
            .as_ref()
            .and_then(|s| s.timeout)
            .unwrap_or(defaults::SAVEPT_TIMEOUT)
    }

    // feishu
    pub fn feishu_config(&self) -> FeishuSection {
        self.file
            .notify
            .as_ref()
            .and_then(|n| n.feishu.clone())
            .unwrap_or_default()
    }
}

static EMPTY_GLOBAL: GlobalSection = GlobalSection {
    concurrency: None,
    batch_size: None,
    api_timeout: None,
    download_timeout: None,
    download_interval: None,
    retry_count: None,
    retry_delay: None,
    mode: None,
};

static EMPTY_DOWNLOADER: DownloaderSection = DownloaderSection {
    dl_type: None,
    host: None,
    port: None,
    username: None,
    password: None,
    torrent_dir: None,
    skip_hash_check: None,
    auto_start: None,
    tag: None,
    source: None,
    destination: None,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloaderRole {
    Source,
    Destination,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloaderKey {
    TorrentDir,
    Tag,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloaderFlag {
    SkipHashCheck,
    AutoStart,
}

#[derive(Debug, Clone)]
pub struct DownloaderConn {
    pub dl_type: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

impl DownloaderConn {
    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }
}

fn load_yaml(path: &Path) -> anyhow::Result<serde_yaml::Value> {
    if !path.exists() {
        anyhow::bail!("配置文件不存在: {}", path.display());
    }
    let text = std::fs::read_to_string(path)?;
    let value: serde_yaml::Value = serde_yaml::from_str(&text)?;
    Ok(value)
}

/// 站点准入过滤（对齐 Python Config.sites 属性）：
/// 1. passkey 或 api_key 至少一项（否则静默跳过）
/// 2. enable 优先于 enabled；都缺省时默认启用
fn filter_enabled_sites(all: Vec<SiteEntry>) -> Vec<SiteEntry> {
    all.into_iter()
        .filter(|s| s.passkey.as_deref().is_some_and(|p| !p.is_empty())
            || s.api_key.as_deref().is_some_and(|k| !k.is_empty()))
        .filter(|s| {
            if let Some(enable) = s.enable {
                enable
            } else {
                s.enabled.unwrap_or(defaults::SITES_ENABLED)
            }
        })
        .collect()
}

impl SiteEntry {
    pub fn effective_cookie(&self) -> Option<&str> {
        self.cookie
            .as_deref()
            .filter(|c| !c.is_empty())
            .or(self.cookies.as_deref().filter(|c| !c.is_empty()))
    }
    pub fn is_hdarea(&self) -> bool {
        self.api_type
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case("hdarea"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(name: &str, content: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("sh_cfg_test_{name}.yaml"));
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn defaults_fill_in_minimal_config() {
        let cfg_path = write_tmp("min_cfg", "global: {concurrency: 7}\n");
        let sites_path = write_tmp(
            "min_sites",
            "sites:\n  - name: a\n    url: https://a.com\n    passkey: p\n",
        );
        let cfg = Config::load(cfg_path.to_str().unwrap(), sites_path.to_str().unwrap()).unwrap();
        assert_eq!(cfg.concurrency(), 7); // 覆盖生效
        assert_eq!(cfg.batch_size(), 100); // 默认值
        assert_eq!(cfg.operation_mode(), "pieces_hash");
        assert_eq!(cfg.retry_count(), 2);
        assert_eq!(cfg.scheduler_interval_seconds(), 360.0 * 60.0);
        assert_eq!(cfg.sites.len(), 1);
    }

    #[test]
    fn site_admission_and_enable_priority() {
        let sites_path = write_tmp(
            "adm",
            "sites:\n\
             - name: has_passkey\n  url: https://a.com\n  passkey: p\n  enabled: false\n\
             - name: enable_wins\n  url: https://b.com\n  passkey: p\n  enable: true\n  enabled: false\n\
             - name: enable_false\n  url: https://c.com\n  passkey: p\n  enable: false\n  enabled: true\n\
             - name: no_credential\n  url: https://d.com\n\
             - name: apikey_only\n  url: https://e.com\n  api_key: k\n  api_type: hdarea\n\
             - name: default_enabled\n  url: https://f.com\n  passkey: p\n",
        );
        let cfg_path = write_tmp("adm_cfg", "{}\n");
        let cfg = Config::load(cfg_path.to_str().unwrap(), sites_path.to_str().unwrap()).unwrap();
        let names: Vec<&str> = cfg.sites.iter().map(|s| s.name.as_str()).collect();
        // has_passkey 被 enabled:false 禁用；no_credential 无凭据静默跳过；
        // enable 优先于 enabled（enable_wins 进、enable_false 出）；缺省默认启用
        assert_eq!(names, vec!["enable_wins", "apikey_only", "default_enabled"]);
        assert!(cfg.sites[1].is_hdarea());
    }

    #[test]
    fn unknown_fields_warned_not_fatal() {
        let cfg_path = write_tmp("unk_cfg", "global: {concurency: 5, mode: jackett}\n");
        let sites_path = write_tmp("unk_sites", "sites: []\n");
        // concurency 是拼错的字段：不阻断、回退默认值
        let cfg = Config::load(cfg_path.to_str().unwrap(), sites_path.to_str().unwrap()).unwrap();
        assert_eq!(cfg.concurrency(), 20);
        assert_eq!(cfg.operation_mode(), "jackett");
    }

    #[test]
    fn dual_downloader_roles() {
        let cfg_path = write_tmp(
            "dual",
            "downloader:\n  \
             source:\n    \
             type: qbittorrent\n    host: 192.168.1.100\n    port: 8080\n    torrent_dir: /src\n  \
             destination:\n    \
             type: transmission\n    host: 127.0.0.1\n    port: 9091\n    tag: T\n",
        );
        let sites_path = write_tmp("dual_sites", "sites: []\n");
        let cfg = Config::load(cfg_path.to_str().unwrap(), sites_path.to_str().unwrap()).unwrap();
        assert!(cfg.is_dual_downloader());
        let src = cfg.downloader_conn(DownloaderRole::Source).unwrap();
        let dst = cfg.downloader_conn(DownloaderRole::Destination).unwrap();
        assert_eq!(src.host, "192.168.1.100");
        assert_eq!(src.dl_type, "qbittorrent");
        assert_eq!(dst.dl_type, "transmission");
        assert_eq!(dst.port, 9091);
        // source 没配 tag，destination 配了 T；source 回退默认 SeedHound
        assert_eq!(cfg.downloader_value(DownloaderKey::Tag, DownloaderRole::Source), "SeedHound");
        assert_eq!(cfg.downloader_value(DownloaderKey::Tag, DownloaderRole::Destination), "T");
    }

    #[test]
    fn missing_file_is_error() {
        let result = Config::load("no/such/file.yaml", "no/such/sites.yaml");
        assert!(result.is_err());
    }
}
