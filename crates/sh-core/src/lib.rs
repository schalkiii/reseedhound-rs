//! sh-core：SeedHound Rust 版核心逻辑层（无网络、无 GUI）。
//!
//! - bencode: 零拷贝游标编解码（info_hash 用原始字节区间，规避重编码差异）
//! - hash: SHA1 hex
//! - parser: torrent 解析 + rayon 并行扫描
//! - config: YAML 配置（DEFAULTS 单一来源 + 未知字段告警 + enable 优先兼容）
//! - cache: SQLite 三表（schema 兼容 Python 版老库）
//! - stats: ReseedStats（GUI 仪表盘共用）
//! - site_table: 四档状态灯 + 排序（三处报告共用口径）
//! - report: 控制台文本报告
//! - redact: 日志凭据脱敏（passkey/api_key/dl_token/X-API-Key）
//! - log_setup: 文件日志 + 轮次裁剪（RUN_MARKER 单一来源）

pub mod bencode;
pub mod cache;
pub mod config;
pub mod hash;
pub mod log_setup;
pub mod parser;
pub mod redact;
pub mod report;
pub mod site_table;
pub mod stats;

pub use parser::{parse_bytes, parse_file, scan_directory, TorrentInfo};
pub use stats::{Phase, ReseedStats};
