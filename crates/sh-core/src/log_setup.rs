//! 日志初始化与轮次裁剪（对齐 Python src/utils/logger.py）。
//!
//! - `setup_logging`：控制台 + 文件双输出，统一脱敏（挂在输出端而非每个调用点）
//! - `trim_log_file`：按 `RUN_MARKER` 保留最近 N 轮，一次性运行防日志无限增长
//!
//! 文件行格式 `2026-11-16 10:00:00 [INFO] 消息` 与 Python logging 的
//! Formatter 一致；weekly-report 的周报正则依赖该格式，改动前先对齐两侧。

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;

use crate::redact::redact_secrets;

/// 一轮辅种在日志中的起始标志。日志裁剪与周报清理共用此常量，
/// 修改日志文案时只需改这一处。
pub const RUN_MARKER: &str = "SeedHound 辅种引擎启动";

/// 按 RUN_MARKER 保留最近 `keep_runs` 轮，更早的行整段删除。
///
/// 与 Python `trim_log_file` 语义一致：轮数不足时不动文件；失败静默
/// （日志系统自身的问题不应打断辅种主流程）。
pub fn trim_log_file(log_path: &str, keep_runs: usize) -> std::io::Result<()> {
    let path = Path::new(log_path);
    if !path.exists() {
        return Ok(());
    }
    let text = fs::read_to_string(path)?;
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let run_starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains(RUN_MARKER))
        .map(|(i, _)| i)
        .collect();
    if run_starts.len() <= keep_runs {
        return Ok(());
    }
    let keep_from = if keep_runs == 0 {
        run_starts[0]
    } else {
        run_starts[run_starts.len() - keep_runs]
    };
    fs::write(path, lines[keep_from..].concat())
}

/// 解析日志级别（缺省 INFO，对齐 Python `getattr(logging, level, INFO)`）。
fn parse_level(level: &str) -> tracing::Level {
    match level.to_ascii_uppercase().as_str() {
        "TRACE" => tracing::Level::TRACE,
        "DEBUG" => tracing::Level::DEBUG,
        "WARN" | "WARNING" => tracing::Level::WARN,
        "ERROR" | "CRITICAL" => tracing::Level::ERROR,
        _ => tracing::Level::INFO,
    }
}

/// 收集事件的格式化字段为消息文本（message 字段即日志正文）。
fn event_message(event: &tracing::Event<'_>) -> String {
    let mut buffer = String::new();
    {
        let mut field_writer = tracing_subscriber::fmt::format::Writer::new(&mut buffer);
        let mut visitor =
            tracing_subscriber::fmt::format::DefaultVisitor::new(field_writer.by_ref(), true);
        event.record(&mut visitor);
    }
    buffer.trim().to_string()
}

/// 输出行：`2026-11-16 10:00:00 [INFO] 脱敏后的消息`。
fn format_log_line(event: &tracing::Event<'_>) -> String {
    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    format!(
        "{} [{}] {}\n",
        timestamp,
        event.metadata().level(),
        redact_secrets(&event_message(event))
    )
}

/// 控制台输出层（stdout，与 Python StreamHandler 一致）。
struct RedactConsoleLayer;

impl<S: tracing::Subscriber> Layer<S> for RedactConsoleLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let line = format_log_line(event);
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        let _ = handle.write_all(line.as_bytes());
        let _ = handle.flush();
    }
}

/// 追加写文件层：UTF-8 追加，进程内共享同一句柄（对齐 Python FileHandler）。
struct RedactFileLayer {
    file: Mutex<File>,
}

impl RedactFileLayer {
    fn new(file: File) -> Self {
        Self {
            file: Mutex::new(file),
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for RedactFileLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let line = format_log_line(event);
        if let Ok(mut file) = self.file.lock() {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

/// 组装订阅器：registry + （可选）文件层，统一级别过滤。
fn build_subscriber(level: &str, log_file: &str) -> Box<dyn tracing::Subscriber + Send + Sync> {
    let filter = tracing_subscriber::filter::LevelFilter::from_level(parse_level(level));
    if log_file.is_empty() {
        return Box::new(
            tracing_subscriber::registry().with(RedactConsoleLayer.with_filter(filter)),
        );
    }
    if let Some(dir) = Path::new(log_file).parent().filter(|d| !d.as_os_str().is_empty()) {
        let _ = fs::create_dir_all(dir);
    }
    match OpenOptions::new().create(true).append(true).open(log_file) {
        Ok(file) => {
            let registry =
                tracing_subscriber::registry().with(RedactConsoleLayer.with_filter(filter));
            Box::new(registry.with(RedactFileLayer::new(file).with_filter(filter)))
        }
        // 文件不可写时退化为仅控制台，不阻断主流程
        Err(_) => Box::new(
            tracing_subscriber::registry().with(RedactConsoleLayer.with_filter(filter)),
        ),
    }
}

/// 初始化全局日志。`keep_runs > 0` 时先裁剪历史（一次性运行保留最近 3 轮；
/// 定时/GUI 常驻传 0 不裁剪，由 weekly-report --clean 负责远期清理）。
pub fn setup_logging(level: &str, log_file: &str, keep_runs: i64) {
    if keep_runs > 0 {
        let _ = trim_log_file(log_file, keep_runs as usize);
    }
    let _ = build_subscriber(level, log_file).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 每个测试独占的临时文件路径（无 tempfile 依赖，用进程内计数器保证唯一）。
    fn temp_log(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "sh-log-{}-{}-{}.log",
            std::process::id(),
            name,
            n
        ))
    }

    fn write_sample_log(path: &Path, runs: usize) {
        let mut text = String::from("进程启动前置行（无时间戳）\n");
        for i in 0..runs {
            text.push_str(&format!(
                "2026-11-01 00:{i:02}:00 [INFO] {RUN_MARKER} [模式: pieces_hash]\n"
            ));
            text.push_str(&format!("2026-11-01 00:{i:02}:30 [INFO] 第 {i} 轮日志\n"));
        }
        fs::write(path, text).unwrap();
    }

    #[test]
    fn trim_keeps_last_three_runs() {
        let path = temp_log("trim3");
        write_sample_log(&path, 5);
        trim_log_file(path.to_str().unwrap(), 3).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        // 3 轮 × 2 行，且从第 2 轮（0 基）的 RUN_MARKER 起
        assert_eq!(lines.len(), 6);
        assert!(lines[0].contains(RUN_MARKER));
        assert!(lines[0].contains("00:02:00"));
        assert!(text.contains("第 4 轮"));
        assert!(!text.contains("第 1 轮"));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn trim_noop_when_runs_not_exceeded() {
        let path = temp_log("noop");
        write_sample_log(&path, 2);
        let before = fs::read_to_string(&path).unwrap();
        trim_log_file(path.to_str().unwrap(), 3).unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(before, after);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn trim_missing_file_is_ok() {
        let missing = std::env::temp_dir().join("sh-log-definitely-missing.log");
        assert!(trim_log_file(missing.to_str().unwrap(), 3).is_ok());
    }

    #[test]
    fn file_layer_redacts_and_formats() {
        let path = temp_log("layer");
        let log_file = path.to_string_lossy().to_string();
        let subscriber = build_subscriber("INFO", &log_file);

        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::info!("下载 https://pt.example.org/download.php?id=1&passkey=SECRET123");
        tracing::debug!("DEBUG 级别不应落盘 passkey=ANOTHER");
        drop(_guard);

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("passkey=***"), "passkey 必须脱敏: {text}");
        assert!(!text.contains("SECRET123"));
        assert!(!text.contains("ANOTHER"), "低于过滤级别的事件不应落盘");
        // 行格式对齐 Python logging：时间戳 [级别] 消息
        assert!(
            text.starts_with("20") && text.contains(" [INFO] 下载 https://pt.example.org"),
            "行格式应为 `时间戳 [级别] 消息`: {text}"
        );
        assert!(text.ends_with('\n'));
        fs::remove_file(&path).ok();
    }
}
