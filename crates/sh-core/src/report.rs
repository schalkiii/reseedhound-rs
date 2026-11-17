//! 文本报告生成（对齐 Python src/report/generator.py）。
//!
//! 中文对齐 padding：display_width 按东亚字符宽 2 计算。

use chrono::Local;

use crate::site_table::{build_site_rows, failed_sites_display};
use crate::stats::ReseedStats;

/// 显示宽度：ASCII 1，其他 2（对齐 Python _display_width）
pub fn display_width(s: &str) -> usize {
    s.chars().map(|c| if (c as u32) < 128 { 1 } else { 2 }).sum()
}

pub fn pad_str(s: &str, width: usize, align: char) -> String {
    let dw = display_width(s);
    if dw >= width {
        return s.to_string();
    }
    let padding = width - dw;
    match align {
        '>' => format!("{}{}", " ".repeat(padding), s),
        '^' => {
            let left = padding / 2;
            let right = padding - left;
            format!("{}{}{}", " ".repeat(left), s, " ".repeat(right))
        }
        _ => format!("{}{}", s, " ".repeat(padding)),
    }
}

pub struct ReportGenerator;

impl ReportGenerator {
    pub fn generate(&self, stats: &ReseedStats) -> String {
        let now = Local::now().format("%Y-%m-%d %H:%M:%S");
        let mut lines: Vec<String> = Vec::new();
        lines.push("=".repeat(50));
        lines.push("  SeedHound 辅种报告".into());
        lines.push(format!("  {now}"));
        lines.push(format!("  运行耗时: {}", stats.duration_str()));
        lines.push("=".repeat(50));
        lines.push(String::new());

        let unique = stats.unique_count();
        let left = [
            ("🟢", "辅种成功", stats.succeeded_count),
            ("🎯", "全站匹配", stats.matched_count),
            ("📦", "扫描种子", stats.total_torrents),
            ("🆕", "新增种子", stats.new_torrents),
        ];
        let right = [
            ("🔴", "下载失败", stats.failed_count),
            ("⏭️", "追踪器跳过", stats.tracker_skip_count),
            ("🔄", "去重", stats.duplicate_count),
            ("📌", "唯一种子", unique),
        ];
        for ((li, ll, lv), (ri, rl, rv)) in left.iter().zip(right.iter()) {
            let lp = format!("{li} {}{}", pad_str(ll, 10, '<'), pad_str(&lv.to_string(), 6, '>'));
            let rp = format!("{ri} {}{}", pad_str(rl, 10, '<'), pad_str(&rv.to_string(), 6, '>'));
            lines.push(format!("  {lp}    {rp}"));
        }
        lines.push(String::new());

        lines.push(format!("[ 站点匹配 ]  共匹配 {} 个种子可跨站辅种", stats.matched_count));

        let rows = build_site_rows(stats);
        if !rows.is_empty() {
            let name_width = rows
                .iter()
                .map(|r| display_width(&r.name))
                .max()
                .unwrap_or(14)
                .max(14);

            lines.push(format!(
                "  | {} | {} | {} | {} | {} |",
                pad_str("状态", 6, '^'),
                pad_str("站点", name_width + 2, '^'),
                pad_str("匹配", 8, '>'),
                pad_str("成功", 8, '>'),
                pad_str("失败", 8, '>'),
            ));
            lines.push(format!(
                "  | {} | {} | {} | {} | {} |",
                pad_str(":--:", 6, '^'),
                pad_str(&format!(":{}", "-".repeat(name_width)), name_width + 2, '^'),
                pad_str(&"-".repeat(7), 8, '>'),
                pad_str(&"-".repeat(7), 8, '>'),
                pad_str(&"-".repeat(7), 8, '>'),
            ));
            for row in &rows {
                lines.push(format!(
                    "  | {} | {} | {} | {} | {} |",
                    pad_str(row.status_icon(), 6, '^'),
                    pad_str(&row.name, name_width + 2, '^'),
                    pad_str(&row.matched.to_string(), 8, '>'),
                    pad_str(&row.succeeded.to_string(), 8, '>'),
                    pad_str(&row.failed.to_string(), 8, '>'),
                ));
            }
        } else {
            lines.push("  (无站点返回匹配数据)".into());
        }
        lines.push(String::new());

        let (dead_count, dead_text) = failed_sites_display(stats);
        if dead_count > 0 {
            lines.push(format!("  ⚠️ 无匹配站点 ({dead_count} 个): {dead_text}"));
            lines.push(String::new());
        }

        let total_sites = stats.site_details.len();
        let active_sites = stats.site_details.values().filter(|c| **c > 0).count();
        let zero_sites = total_sites - active_sites;
        lines.push(format!(
            "[ 站点汇总 ]  启用 {total_sites} 个  |  有匹配 {active_sites} 个  |  无匹配 {zero_sites} 个"
        ));
        lines.push(String::new());
        lines.push("=".repeat(50));
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_width_cjk_is_two() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("站点"), 4);
        assert_eq!(display_width("a站"), 3);
    }

    #[test]
    fn pad_str_aligns() {
        assert_eq!(pad_str("ab", 4, '<'), "ab  ");
        assert_eq!(pad_str("ab", 4, '>'), "  ab");
        assert_eq!(pad_str("ab", 5, '^'), " ab  ");
        // CJK 宽度对齐
        assert_eq!(pad_str("站点", 6, '<'), "站点  ");
    }

    #[test]
    fn report_contains_all_sections() {
        let stats = ReseedStats {
            start_time: 0.0,
            end_time: 30.0,
            total_torrents: 100,
            duplicate_count: 20,
            new_torrents: 10,
            matched_count: 5,
            succeeded_count: 3,
            site_details: [("siteA".into(), 5)].into(),
            site_match_counts: [("siteA".into(), 5)].into(),
            site_succeeded: [("siteA".into(), 3)].into(),
            ..Default::default()
        };

        let report = ReportGenerator.generate(&stats);
        assert!(report.contains("SeedHound 辅种报告"));
        assert!(report.contains("30秒"));
        assert!(report.contains("siteA"));
        assert!(report.contains("[ 站点汇总 ]"));
        assert!(report.contains("有匹配 1 个"));
    }
}
