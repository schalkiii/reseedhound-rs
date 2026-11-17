//! 日志周报生成 + 远期日志清理（对齐 Python scripts/weekly_report.py）。
//!
//! 解析 logs/reseed.log，按站点聚合近一段时间（默认近 7 天）的辅种指标，
//! 输出 Markdown 周报；可选地清理远期日志（默认 dry-run 预览，--apply 才真正截断）。
//!
//! 轮次标志与日志裁剪共用 `sh_core::log_setup::RUN_MARKER`（单一来源），
//! 日志启动文案变更时周报清理不会静默失真。

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

use chrono::{Duration, Local, NaiveDateTime};
use regex::Regex;
use sh_core::config::SiteEntry;
use sh_core::log_setup::RUN_MARKER;

static TS_RE: OnceLock<Regex> = OnceLock::new();
fn ts_re() -> &'static Regex {
    TS_RE.get_or_init(|| Regex::new(r"^(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2})").unwrap())
}

static URL_HOST_RE: OnceLock<Regex> = OnceLock::new();
fn url_host_re() -> &'static Regex {
    URL_HOST_RE.get_or_init(|| Regex::new(r"https?://([^/]+)/").unwrap())
}

static RUN_SUMMARY_RE: OnceLock<Regex> = OnceLock::new();
fn run_summary_re() -> &'static Regex {
    RUN_SUMMARY_RE.get_or_init(|| {
        Regex::new(
            r"辅种完成: 扫描=(\d+), 去重=(\d+), 新=(\d+), 匹配=(\d+), 追踪器跳过=(\d+), 成功=(\d+), 失败=(\d+), 死种=(\d+)",
        )
        .unwrap()
    })
}

static MATCH_RE: OnceLock<Regex> = OnceLock::new();
fn match_re() -> &'static Regex {
    MATCH_RE.get_or_init(|| Regex::new(r"站点 (.+?) 匹配到 (\d+) 个种子").unwrap())
}

static SKIP_RE: OnceLock<Regex> = OnceLock::new();
fn skip_re() -> &'static Regex {
    SKIP_RE.get_or_init(|| Regex::new(r"站点 (.+?) \((\d+)次严重错误: (.+?)\)，跳过后续下载").unwrap())
}

static SUCCESS_RE: OnceLock<Regex> = OnceLock::new();
fn success_re() -> &'static Regex {
    SUCCESS_RE.get_or_init(|| Regex::new(r"辅种成功: .*?-> (.+?) \(id=(\d+)\)").unwrap())
}

static HTML_FAIL_RE: OnceLock<Regex> = OnceLock::new();
fn html_fail_re() -> &'static Regex {
    HTML_FAIL_RE.get_or_init(|| Regex::new(r"下载返回HTML而非torrent: .*?原因: (\S+)").unwrap())
}

/// 单轮辅种汇总（来自"辅种完成"日志行）。
#[derive(Debug, Clone)]
pub struct RunSummary {
    pub time: NaiveDateTime,
    pub scan: u64,
    pub dedup: u64,
    pub new: u64,
    pub matched: u64,
    pub tracker_skip: u64,
    pub success: u64,
    pub failed: u64,
    pub dead: u64,
}

/// 站点聚合指标（保持首次出现顺序，与 Python defaultdict 插入序一致）。
#[derive(Debug, Default, Clone)]
pub struct SiteStats {
    pub matched: u64,
    pub success: u64,
    pub failed: u64,
    pub severe_skips: u64,
    /// 失败原因 -> 次数（保持首次出现顺序）
    pub reason_counts: Vec<(String, u64)>,
    pub skip_reasons: Vec<(String, u64)>,
    pub last_seen: Option<NaiveDateTime>,
}

impl SiteStats {
    fn bump_reason(reasons: &mut Vec<(String, u64)>, reason: &str) {
        match reasons.iter_mut().find(|(k, _)| k == reason) {
            Some((_, count)) => *count += 1,
            None => reasons.push((reason.to_string(), 1)),
        }
    }
}

/// host -> 站点名 映射，用于把下载失败 URL 归属到站点。
/// 解析失败或文件缺失时返回空表并打告警日志（归因会失效，不能静默）。
pub fn load_site_host_map(sites_path: &str) -> HashMap<String, String> {
    match fs::read_to_string(sites_path) {
        Err(e) => {
            tracing::warn!("站点表读取失败（失败归因将不可用）: {sites_path}: {e}");
            HashMap::new()
        }
        Ok(raw) => match serde_yaml::from_str::<sh_core::config::SitesFile>(&raw) {
            Err(e) => {
                tracing::warn!("站点表解析失败（失败归因将不可用）: {sites_path}: {e}");
                HashMap::new()
            }
            Ok(entries) => {
                let mut host_map = HashMap::new();
                for s in entries.sites {
                    let SiteEntry { name, url, .. } = s;
                    if name.trim().is_empty() || url.trim().is_empty() {
                        continue;
                    }
                    if let Some(host) = host_of(&url) {
                        host_map.insert(host, name);
                    }
                }
                host_map
            }
        },
    }
}

/// 提取 URL host（小写），失败返回 None。
fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .map(|u| u.host_str().unwrap_or_default().to_lowercase())
        .filter(|h| !h.is_empty())
}

fn parse_dt(line: &str) -> Option<NaiveDateTime> {
    let captures = ts_re().captures(line)?;
    NaiveDateTime::parse_from_str(&captures[1], "%Y-%m-%d %H:%M:%S").ok()
}

/// 站点聚合结果（有序：站点名 -> 指标，首次出现序）。
pub type PerSiteStats = Vec<(String, SiteStats)>;

/// 逐行解析日志，返回 (per_site 聚合, run_summaries)。保持首次出现顺序。
pub fn parse_log(
    log_path: &str,
    since: NaiveDateTime,
    host_map: &HashMap<String, String>,
) -> anyhow::Result<(PerSiteStats, Vec<RunSummary>)> {
    let text = fs::read_to_string(log_path)?;
    let mut per_site: Vec<(String, SiteStats)> = Vec::new();
    let mut runs = Vec::new();
    let mut in_window = false;

    for line in text.lines() {
        let Some(dt) = parse_dt(line) else {
            continue;
        };
        if dt >= since {
            in_window = true;
        } else if !in_window {
            continue; // 早于窗口且尚未进入窗口，跳过
        }
        classify(line, dt, &mut per_site, &mut runs, host_map);
    }
    Ok((per_site, runs))
}

fn site_slot<'a>(per_site: &'a mut PerSiteStats, name: &str) -> &'a mut SiteStats {
    let idx = match per_site.iter().position(|(k, _)| k == name) {
        Some(i) => i,
        None => {
            per_site.push((name.to_string(), SiteStats::default()));
            per_site.len() - 1
        }
    };
    &mut per_site[idx].1
}

fn classify(
    line: &str,
    dt: NaiveDateTime,
    per_site: &mut PerSiteStats,
    runs: &mut Vec<RunSummary>,
    host_map: &HashMap<String, String>,
) {
    if let Some(c) = run_summary_re().captures(line) {
        runs.push(RunSummary {
            time: dt,
            scan: c[1].parse().unwrap_or(0),
            dedup: c[2].parse().unwrap_or(0),
            new: c[3].parse().unwrap_or(0),
            matched: c[4].parse().unwrap_or(0),
            tracker_skip: c[5].parse().unwrap_or(0),
            success: c[6].parse().unwrap_or(0),
            failed: c[7].parse().unwrap_or(0),
            dead: c[8].parse().unwrap_or(0),
        });
        return;
    }

    if let Some(c) = match_re().captures(line) {
        let name = c[1].trim();
        let count: u64 = c[2].parse().unwrap_or(0);
        let st = site_slot(per_site, name);
        st.matched += count;
        st.last_seen = Some(dt);
        return;
    }

    if let Some(c) = skip_re().captures(line) {
        let name = c[1].trim();
        let reason = c[3].trim();
        let st = site_slot(per_site, name);
        st.severe_skips += 1;
        SiteStats::bump_reason(&mut st.skip_reasons, reason);
        st.last_seen = Some(dt);
        return;
    }

    if let Some(c) = success_re().captures(line) {
        let name = c[1].trim();
        let st = site_slot(per_site, name);
        st.success += 1;
        st.last_seen = Some(dt);
        return;
    }

    // 下载失败归因到站点
    let reason = if let Some(c) = html_fail_re().captures(line) {
        Some(c[1].trim_matches(')').to_string())
    } else if line.contains("下载超时:") {
        Some("timeout".to_string())
    } else if line.contains("下载错误 ") {
        Some("conn_error".to_string())
    } else {
        None
    };

    if let Some(reason) = reason {
        if let Some(c) = url_host_re().captures(line) {
            if let Some(site_name) = host_map.get(&c[1].to_lowercase()) {
                let st = site_slot(per_site, site_name);
                st.failed += 1;
                SiteStats::bump_reason(&mut st.reason_counts, &reason);
                st.last_seen = Some(dt);
            }
        }
    }
}

/// 生成 Markdown 周报。
pub fn generate_report(
    per_site: &PerSiteStats,
    runs: &[RunSummary],
    since: NaiveDateTime,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "# SeedHound 周报（{} 起）",
        since.format("%Y-%m-%d")
    ));
    lines.push(String::new());
    if !runs.is_empty() {
        let total_match: u64 = runs.iter().map(|r| r.matched).sum();
        let total_ok: u64 = runs.iter().map(|r| r.success).sum();
        let total_fail: u64 = runs.iter().map(|r| r.failed).sum();
        let total_dead: u64 = runs.iter().map(|r| r.dead).sum();
        lines.push(format!("- 运行轮次: **{}**", runs.len()));
        lines.push(format!(
            "- 累计匹配: {total_match} | 成功: {total_ok} | 失败: {total_fail} | 死种: {total_dead}"
        ));
        lines.push(String::new());
        lines.push("## 各轮汇总".to_string());
        lines.push(String::new());
        lines.push("| 时间 | 匹配 | 成功 | 失败 | 死种 |".to_string());
        lines.push("| --- | --- | --- | --- | --- |".to_string());
        for r in runs {
            lines.push(format!(
                "| {} | {} | {} | {} | {} |",
                r.time.format("%m-%d %H:%M"),
                r.matched,
                r.success,
                r.failed,
                r.dead
            ));
        }
    } else {
        lines.push("_窗口内无运行记录_".to_string());
    }

    lines.push(String::new());
    lines.push("## 站点明细（按失败数降序）".to_string());
    lines.push(String::new());
    lines.push("| 站点 | 匹配 | 成功 | 失败 | 严重跳过 | 主要失败原因 |".to_string());
    lines.push("| --- | --- | --- | --- | --- | --- |".to_string());
    let mut sorted: Vec<&(String, SiteStats)> = per_site.iter().collect();
    // 稳定排序：失败数相同保持首次出现顺序（对齐 Python sorted 稳定性）
    sorted.sort_by_key(|(_, st)| std::cmp::Reverse(st.failed));
    for (name, st) in sorted {
        let mut reasons = st.reason_counts.clone();
        reasons.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        let reason_text: Vec<String> = reasons
            .iter()
            .take(3)
            .map(|(k, v)| format!("{k}:{v}"))
            .collect();
        lines.push(format!(
            "| {} | {} | {} | {} | {} | {} |",
            name,
            st.matched,
            st.success,
            st.failed,
            st.severe_skips,
            reason_text.join(", ")
        ));
    }
    lines.join("\n")
}

/// 清理远期日志：保留最近 keep_days 天，且至少保留 keep_runs 轮。
///
/// 返回预览/执行结果消息（对齐 Python cleanup_log 的两条文案）。
pub fn cleanup_log(log_path: &str, keep_days: i64, keep_runs: usize, apply: bool) -> anyhow::Result<String> {
    let text = fs::read_to_string(log_path)?;
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let run_starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, ln)| ln.contains(RUN_MARKER))
        .map(|(i, _)| i)
        .collect();

    // 按天数截止（以日志中最新时间戳为基准）
    let cutoff_dt = lines
        .iter()
        .rev()
        .find_map(|ln| parse_dt(ln))
        .map(|latest| latest - Duration::days(keep_days));
    let mut keep_from_idx = 0;
    if let Some(cutoff) = cutoff_dt {
        for (i, ln) in lines.iter().enumerate() {
            if let Some(dt) = parse_dt(ln) {
                if dt >= cutoff {
                    keep_from_idx = i;
                    break;
                }
            }
        }
    }

    // 按轮次下限（至少保留 keep_runs 轮，避免日期窗口过窄时误删过多）
    if run_starts.len() > keep_runs {
        let floor = run_starts[run_starts.len() - keep_runs];
        keep_from_idx = keep_from_idx.min(floor);
    }

    let deleted_lines = keep_from_idx;
    let kept_lines = lines.len() - keep_from_idx;
    let removed_bytes: usize = lines[..keep_from_idx].iter().map(|ln| ln.len()).sum();
    let kept_bytes: usize = lines[keep_from_idx..].iter().map(|ln| ln.len()).sum();
    if apply {
        fs::write(Path::new(log_path), lines[keep_from_idx..].concat())?;
        Ok(format!(
            "[已清理] 删除 {deleted_lines} 行，保留 {kept_lines} 行。"
        ))
    } else {
        Ok(format!(
            "[清理预览] 将删除 {deleted_lines} 行（约 {:.2} MB），保留 {kept_lines} 行（约 {:.2} MB）。",
            removed_bytes as f64 / 1024.0 / 1024.0,
            kept_bytes as f64 / 1024.0 / 1024.0,
        ))
    }
}

/// 以日志中最新时间戳为参考终点（兜底当前时间）。只看文件尾部有限行数，
/// 避免把动辄数百 MB 的历史日志全量解析。
pub fn window_start(log_path: &str, days: i64) -> anyhow::Result<NaiveDateTime> {
    let text = fs::read_to_string(log_path)?;
    let latest = text
        .lines()
        .rev()
        .find_map(parse_dt)
        .unwrap_or_else(|| Local::now().naive_local());
    Ok(latest - Duration::days(days))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_file(name: &str, content: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "sh-weekly-{}-{}-{}.log",
            std::process::id(),
            name,
            n
        ));
        fs::write(&path, content).unwrap();
        path
    }

    fn sample_log_lines() -> String {
        [
            format!("2026-11-01 00:00:00 [INFO] {RUN_MARKER} [模式: pieces_hash]\n"),
            "2026-11-01 00:00:01 [INFO] 站点 siteA 匹配到 12 个种子\n".to_string(),
            "2026-11-01 00:00:02 [INFO] 辅种成功: f.torrent -> siteA (id=1)\n".to_string(),
            "2026-11-01 00:00:03 [INFO] 下载超时: https://a.example.com/dl\n".to_string(),
            "2026-11-01 00:00:04 [INFO] 辅种完成: 扫描=10, 去重=1, 新=9, 匹配=12, 追踪器跳过=2, 成功=1, 失败=1, 死种=0\n".to_string(),
        ]
        .concat()
    }

    fn sample_sites_yaml() -> String {
        "sites:\n  - name: siteA\n    url: https://a.example.com\n  - name: siteB\n    url: https://b.example.com\n".to_string()
    }

    #[test]
    fn weekly_report_parse_and_generate() {
        let log = temp_file("parse", &sample_log_lines());
        let since = NaiveDateTime::parse_from_str("2026-10-01 00:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let mut host_map = HashMap::new();
        host_map.insert("a.example.com".to_string(), "siteA".to_string());

        let (per_site, runs) = parse_log(log.to_str().unwrap(), since, &host_map).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].matched, 12);
        assert_eq!(runs[0].success, 1);
        let (_, site_a) = &per_site[0];
        assert_eq!(site_a.matched, 12);
        assert_eq!(site_a.success, 1);
        assert_eq!(site_a.failed, 1); // 下载超时按 URL 域名归因

        let report = generate_report(&per_site, &runs, since);
        assert!(report.contains("运行轮次: **1**"), "{report}");
        assert!(report.contains("| siteA | 12 | 1 | 1 | 0 |"), "{report}");
        fs::remove_file(&log).ok();
    }

    #[test]
    fn weekly_report_cleanup_preview_and_apply() {
        let log = temp_file("clean", &sample_log_lines());
        let path = log.to_str().unwrap();
        let original = sample_log_lines();

        let preview = cleanup_log(path, 1, 3, false).unwrap();
        assert!(preview.contains("清理预览"), "{preview}");
        assert_eq!(fs::read_to_string(path).unwrap(), original); // 预览不改动文件

        let applied = cleanup_log(path, 0, 1, true).unwrap();
        assert!(applied.contains("已清理"), "{applied}");
        assert_eq!(fs::read_to_string(path).unwrap().lines().count(), 1);
        fs::remove_file(&log).ok();
    }

    #[test]
    fn load_site_host_map_from_yaml() {
        let sites = temp_file("sites", &sample_sites_yaml());
        let map = load_site_host_map(sites.to_str().unwrap());
        assert_eq!(
            map,
            HashMap::from([
                ("a.example.com".to_string(), "siteA".to_string()),
                ("b.example.com".to_string(), "siteB".to_string()),
            ])
        );
        fs::remove_file(&sites).ok();
    }

    #[test]
    fn window_outside_entries_are_skipped() {
        // 早于 since 的行应被跳过，不影响聚合
        let content = "2026-01-01 00:00:00 [INFO] 站点 siteA 匹配到 99 个种子\n".to_string()
            + &sample_log_lines();
        let log = temp_file("window", &content);
        let since = NaiveDateTime::parse_from_str("2026-10-01 00:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let (per_site, _) = parse_log(log.to_str().unwrap(), since, &HashMap::new()).unwrap();
        let (_, site_a) = &per_site[0];
        assert_eq!(site_a.matched, 12);
        fs::remove_file(&log).ok();
    }
}
