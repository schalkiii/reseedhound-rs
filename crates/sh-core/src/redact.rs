//! 日志凭据脱敏（对齐 Python src/utils/logger.py 的 redact_secrets）。
//!
//! 下载链接形如 `.../download.php?id=1&passkey=xxxx`，passkey 等同于账号权限；
//! HDArea 通道另外引入 apikey 查询参数、dl_token 短效凭证与 X-API-Key 请求头，
//! 一并纳入统一脱敏，避免新增日志点各自处理。
//!
//! 脱敏在 `log_setup` 的输出层统一执行（对齐 Python 挂在 handler 上的
//! `_RedactSecretsFilter`），不依赖每个调用点自觉。

use std::sync::OnceLock;

use regex::Regex;

fn secret_patterns() -> &'static Vec<Regex> {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        vec![
            // (passkey=)xxx —— 值截至 & / 空白 / 引号
            Regex::new(r#"(?i)(passkey=)[^&\s"']+"#).unwrap(),
            // (api_key= / apikey= / api-key=)xxx
            Regex::new(r#"(?i)(api[_-]?key=)[^&\s"']+"#).unwrap(),
            // (dl_token=)xxx
            Regex::new(r#"(?i)(dl_token=)[^&\s"']+"#).unwrap(),
            // (X-API-Key: )xxx
            Regex::new(r"(?i)(X-API-Key:\s*)\S+").unwrap(),
        ]
    })
}

/// 把日志文本里的 passkey / API Key / 短效下载凭证打码。
pub fn redact_secrets(text: &str) -> String {
    let mut redacted = text.to_string();
    for pattern in secret_patterns() {
        redacted = pattern.replace_all(&redacted, "${1}***").to_string();
    }
    redacted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_download_url_passkey() {
        let raw = "https://pt.example.org/download.php?id=123&passkey=abcdef123456";
        assert_eq!(
            redact_secrets(raw),
            "https://pt.example.org/download.php?id=123&passkey=***"
        );
    }

    #[test]
    fn redacts_all_variants() {
        assert_eq!(
            redact_secrets("x?api_key=SECRET&dl_token=TOKEN"),
            "x?api_key=***&dl_token=***"
        );
        assert_eq!(redact_secrets("x?apikey=SECRET"), "x?apikey=***");
        assert_eq!(redact_secrets("x?api-key=SECRET"), "x?api-key=***");
        assert_eq!(redact_secrets("X-API-Key: abc123xyz"), "X-API-Key: ***");
        assert_eq!(redact_secrets("x-api-key: abc"), "x-api-key: ***");
    }

    #[test]
    fn keeps_plain_text() {
        assert_eq!(redact_secrets("普通日志，无凭据"), "普通日志，无凭据");
        assert_eq!(redact_secrets("passkey=abc"), "passkey=***");
        // 非键值形式的 passkey 字样不打码
        assert_eq!(redact_secrets("读取 passkey 配置"), "读取 passkey 配置");
    }

    #[test]
    fn quoted_value_boundary() {
        assert_eq!(
            redact_secrets("url=\"https://x.org/dl?id=1&passkey=pk123\""),
            "url=\"https://x.org/dl?id=1&passkey=***\""
        );
    }
}
