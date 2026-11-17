//! 辅种统计（对齐 Python seeder.ReseedStats）。
//!
//! 这是 GUI 仪表盘与文本/飞书报告共用的数据结构，
//! 字段口径见 PYTHON-INDEX §6。

use std::collections::HashMap;

use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize)]
pub struct ReseedStats {
    pub total_torrents: u64,
    pub new_torrents: u64,
    pub duplicate_count: u64,
    pub tracker_skip_count: u64,
    pub matched_count: u64,
    pub succeeded_count: u64,
    pub failed_count: u64,
    pub dead_count: u64,
    pub skipped_count: u64,
    pub site_details: HashMap<String, u64>,
    pub site_match_counts: HashMap<String, u64>,
    pub site_succeeded: HashMap<String, u64>,
    pub site_failed: HashMap<String, u64>,
    pub failed_sites: Vec<String>,
    /// 站点名 -> 首个凭据类错误原因
    pub credential_failures: HashMap<String, String>,
    pub start_time: f64,
    pub end_time: f64,
    /// 阶段进度（GUI 仪表盘用；Python 版无此字段，纯增补）
    pub phase: Phase,
}

/// 当前流水线阶段（GUI 进度显示用）
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Idle,
    Scan,
    Query,
    Add,
    Done,
}

impl ReseedStats {
    pub fn duration_seconds(&self) -> f64 {
        self.end_time - self.start_time
    }

    /// 对齐 Python duration_str：X秒 / X分Y秒
    pub fn duration_str(&self) -> String {
        let secs = self.duration_seconds() as u64;
        if secs < 60 {
            return format!("{secs}秒");
        }
        let (mins, secs) = (secs / 60, secs % 60);
        format!("{mins}分{secs}秒")
    }

    pub fn unique_count(&self) -> u64 {
        self.total_torrents - self.duplicate_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_str_matches_python_format() {
        let s = ReseedStats { start_time: 0.0, end_time: 45.0, ..Default::default() };
        assert_eq!(s.duration_str(), "45秒");
        let s = ReseedStats { start_time: 0.0, end_time: 125.0, ..Default::default() };
        assert_eq!(s.duration_str(), "2分5秒");
    }

    #[test]
    fn unique_count() {
        let s = ReseedStats {
            total_torrents: 100,
            duplicate_count: 30,
            ..Default::default()
        };
        assert_eq!(s.unique_count(), 70);
    }
}
