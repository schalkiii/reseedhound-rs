//! 站点明细表统一口径（对齐 Python src/report/site_table.py）。
//!
//! 四档状态灯 + 排序规则（成功率↓ → succeeded↓ → matched↓ → name↑）。
//! 文本报告、飞书卡片、GPUI 仪表盘三处共用，保证同一轮辅种结论一致。

use crate::stats::ReseedStats;

pub const STATUS_ALL_OK: &str = "🟢";
pub const STATUS_PARTIAL: &str = "🟡";
pub const STATUS_MATCH_ONLY: &str = "🔵";
pub const STATUS_NONE: &str = "⚪";

/// 报告里最多列出的无匹配站点数
pub const MAX_FAILED_SITES_DISPLAY: usize = 10;

#[derive(Debug, Clone, PartialEq)]
pub struct SiteRow {
    pub name: String,
    pub matched: u64,
    pub succeeded: u64,
    pub failed: u64,
}

impl SiteRow {
    pub fn success_rate(&self) -> f64 {
        let total = self.succeeded + self.failed;
        if total > 0 {
            self.succeeded as f64 / total as f64
        } else {
            0.0
        }
    }

    pub fn status_icon(&self) -> &'static str {
        if self.succeeded > 0 && self.failed == 0 {
            STATUS_ALL_OK
        } else if self.succeeded > 0 {
            STATUS_PARTIAL
        } else if self.matched > 0 {
            STATUS_MATCH_ONLY
        } else {
            STATUS_NONE
        }
    }
}

pub fn build_site_rows(stats: &ReseedStats) -> Vec<SiteRow> {
    let mut names: std::collections::BTreeSet<&String> = Default::default();
    names.extend(stats.site_match_counts.keys());
    names.extend(stats.site_succeeded.keys());
    names.extend(stats.site_failed.keys());

    let mut rows: Vec<SiteRow> = names
        .into_iter()
        .map(|name| SiteRow {
            name: name.clone(),
            matched: stats.site_match_counts.get(name).copied().unwrap_or(0),
            succeeded: stats.site_succeeded.get(name).copied().unwrap_or(0),
            failed: stats.site_failed.get(name).copied().unwrap_or(0),
        })
        .filter(|r| r.matched > 0 || r.succeeded > 0 || r.failed > 0)
        .collect();

    rows.sort_by(|a, b| {
        b.success_rate()
            .partial_cmp(&a.success_rate())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.succeeded.cmp(&a.succeeded))
            .then(b.matched.cmp(&a.matched))
            .then(a.name.cmp(&b.name))
    });
    rows
}

/// (无匹配站点总数, 展示文案)
pub fn failed_sites_display(stats: &ReseedStats) -> (usize, String) {
    let sites = &stats.failed_sites;
    let shown: Vec<&str> = sites.iter().take(MAX_FAILED_SITES_DISPLAY).map(|s| s.as_str()).collect();
    let mut text = shown.join(", ");
    if sites.len() > shown.len() {
        text += &format!(" 等 {} 个", sites.len());
    }
    (sites.len(), text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats_with(
        match_counts: &[(&str, u64)],
        succeeded: &[(&str, u64)],
        failed: &[(&str, u64)],
    ) -> ReseedStats {
        ReseedStats {
            site_match_counts: match_counts.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            site_succeeded: succeeded.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            site_failed: failed.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn status_icons_four_tiers() {
        let all_ok = SiteRow { name: "a".into(), matched: 5, succeeded: 3, failed: 0 };
        let partial = SiteRow { name: "b".into(), matched: 5, succeeded: 2, failed: 1 };
        let match_only = SiteRow { name: "c".into(), matched: 5, succeeded: 0, failed: 0 };
        let none = SiteRow { name: "d".into(), matched: 0, succeeded: 0, failed: 0 };
        assert_eq!(all_ok.status_icon(), STATUS_ALL_OK);
        assert_eq!(partial.status_icon(), STATUS_PARTIAL);
        assert_eq!(match_only.status_icon(), STATUS_MATCH_ONLY);
        assert_eq!(none.status_icon(), STATUS_NONE);
    }

    #[test]
    fn sort_by_rate_then_counts_then_name() {
        let s = stats_with(
            &[("low_rate", 10), ("high_rate", 10), ("same_rate_b", 5), ("same_rate_a", 5)],
            &[("low_rate", 1), ("high_rate", 9), ("same_rate_b", 3), ("same_rate_a", 3)],
            &[("low_rate", 9), ("high_rate", 1), ("same_rate_b", 1), ("same_rate_a", 1)],
        );
        let rows = build_site_rows(&s);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        // high_rate(90%) > same_rate_a/b(75%, 同率按名) > low_rate(10%)
        assert_eq!(names, vec!["high_rate", "same_rate_a", "same_rate_b", "low_rate"]);
    }

    #[test]
    fn failed_sites_display_truncates() {
        let s = ReseedStats {
            failed_sites: (0..15).map(|i| format!("s{i}")).collect(),
            ..Default::default()
        };
        let (count, text) = failed_sites_display(&s);
        assert_eq!(count, 15);
        assert!(text.ends_with("等 15 个"));
        assert!(text.contains("s9"));
        assert!(!text.contains("s10,"));
    }
}
