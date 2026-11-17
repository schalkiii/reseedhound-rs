//! 飞书通知：交互卡片（辅种报告 + 凭据失效提醒）。
//!
//! 对齐 Python src/notify/feishu.py：
//! - 报告卡片：总览指标 + 站点表格 + 底部 note（耗时/站点数/命中/死种）
//! - 凭据提醒卡片（红色 template）
//! - 签名：HMAC-SHA256(key="{timestamp}\n{secret}", msg="") + base64
//! - 响应 code==0 或 StatusCode==0 视为成功

use base64::Engine;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;

use sh_core::stats::ReseedStats;

type HmacSha256 = Hmac<Sha256>;

pub struct FeishuNotifier {
    webhook_url: String,
    secret: Option<String>,
    http: reqwest::Client,
}

impl FeishuNotifier {
    pub fn new(webhook_url: &str, secret: Option<&str>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("reqwest client");
        Self {
            webhook_url: webhook_url.to_string(),
            secret: secret.map(|s| s.to_string()),
            http,
        }
    }

    pub fn from_config(webhook_url: &str, secret: Option<&str>) -> Option<Self> {
        if webhook_url.is_empty() {
            tracing::warn!("飞书 webhook_url 未配置，跳过通知");
            return None;
        }
        Some(Self::new(webhook_url, secret))
    }

    /// 飞书自定义机器人签名。
    fn gen_sign(timestamp: &str, secret: &str) -> String {
        let string_to_sign = format!("{timestamp}\n{secret}");
        let mut mac = HmacSha256::new_from_slice(string_to_sign.as_bytes()).expect("hmac key");
        // Python: hmac.new(key, digestmod=sha256).digest() —— 空 message
        mac.update(b"");
        let digest = mac.finalize().into_bytes();
        base64::engine::general_purpose::STANDARD.encode(digest)
    }

    async fn send(&self, mut payload: Value) -> bool {
        if let Some(secret) = &self.secret {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let ts = timestamp.to_string();
            let sign = Self::gen_sign(&ts, secret);
            payload["timestamp"] = Value::String(ts);
            payload["sign"] = Value::String(sign);
        }
        match self
            .http
            .post(&self.webhook_url)
            .json(&payload)
            .send()
            .await
        {
            Ok(resp) => match resp.json::<Value>().await {
                Ok(body) => {
                    let code = body
                        .get("code")
                        .and_then(|c| c.as_i64())
                        .unwrap_or_else(|| {
                            body.get("StatusCode")
                                .and_then(|c| c.as_i64())
                                .unwrap_or(-1)
                        });
                    if code == 0 {
                        tracing::info!("飞书消息发送成功");
                        true
                    } else {
                        tracing::warn!("飞书消息发送失败: {body}");
                        false
                    }
                }
                Err(e) => {
                    tracing::error!("飞书消息发送异常: {e}");
                    false
                }
            },
            Err(e) => {
                tracing::error!("飞书消息发送异常: {e}");
                false
            }
        }
    }

    /// 发送辅种报告卡片。
    pub async fn send_report_card(&self, stats: &ReseedStats) -> bool {
        let elements = build_card_elements(stats);
        let payload = json!({
            "msg_type": "interactive",
            "card": {
                "header": {
                    "title": {"tag": "plain_text", "content": "SeedHound 辅种报告"},
                    "template": "blue",
                },
                "elements": elements,
            },
        });
        self.send(payload).await
    }

    /// 凭据失效提醒（红色卡片）。
    pub async fn send_credential_alert(
        &self,
        failures: &std::collections::HashMap<String, String>,
    ) -> bool {
        if failures.is_empty() {
            return false;
        }
        let mut lines: Vec<String> = failures
            .iter()
            .map(|(site, reason)| format!("- **{site}**：`{reason}`"))
            .collect();
        lines.sort();
        let content = format!(
            "**⚠️ 凭据失效提醒**\n\n\
             以下站点的 passkey / Cookie 已失效或权限不足，辅种下载被拒。\
             请到站点重新获取凭据（或运行 Cookie 同步）：\n\n{}\n\n\
             共 **{}** 个站点需要更新凭据。",
            lines.join("\n"),
            failures.len()
        );
        let payload = json!({
            "msg_type": "interactive",
            "card": {
                "header": {
                    "title": {"tag": "plain_text", "content": "SeedHound 凭据失效提醒"},
                    "template": "red",
                },
                "elements": [{"tag": "markdown", "content": content}],
            },
        });
        self.send(payload).await
    }
}

/// 构建报告卡片 elements（对齐 Python _build_card_elements）。
fn build_card_elements(stats: &ReseedStats) -> Vec<Value> {
    let mut elements = Vec::new();
    let unique_count = stats.unique_count();

    let status_icon = if stats.succeeded_count > 0 {
        "✅"
    } else if stats.matched_count > 0 {
        "⚠️"
    } else {
        "❌"
    };

    elements.push(json!({
        "tag": "markdown",
        "content": format!(
            "**{status_icon} 总览**\n\n\
             🟢 辅种成功　**{}**　　　　🔴 下载失败　**{}**\n\
             🎯 全站匹配　**{}**　　　　⏭️ 追踪器跳过 **{}**\n\
             📦 扫描种子　**{}**　　　　🔄 去重　　　**{}**\n\
             🆕 新增种子　**{}**　　　　　📌 唯一种子　**{unique_count}**",
            stats.succeeded_count, stats.failed_count,
            stats.matched_count, stats.tracker_skip_count,
            stats.total_torrents, stats.duplicate_count,
            stats.new_torrents,
        ),
    }));

    let rows = sh_core::site_table::build_site_rows(stats);
    if !rows.is_empty() {
        elements.push(json!({"tag": "hr"}));
        let mut table_lines = vec![
            "**📈 站点详情**\n".to_string(),
            "| 状态 | 站点 | 匹配 | 成功 | 失败 |".to_string(),
            "| :---: | :--- | ---: | ---: | ---: |".to_string(),
        ];
        for row in rows {
            table_lines.push(format!(
                "| {} | {} | {} | {} | {} |",
                row.status_icon(),
                row.name,
                row.matched,
                row.succeeded,
                row.failed
            ));
        }
        elements.push(json!({
            "tag": "markdown",
            "content": table_lines.join("\n"),
        }));
    }

    elements.push(json!({"tag": "hr"}));

    let site_total = stats.site_details.len();
    let active = stats.site_details.values().filter(|c| **c > 0).count();
    let (dead_count, dead_text) = sh_core::site_table::failed_sites_display(stats);

    let mut footer_parts = vec![format!("⏱ 耗时 **{}**", stats.duration_str())];
    footer_parts.push(format!("📡 {site_total} 站点"));
    footer_parts.push(format!("🎯 {active} 命中"));
    footer_parts.push(format!("❌ {dead_count} 无匹配"));
    if stats.dead_count > 0 {
        footer_parts.push(format!("💀 {} 死种", stats.dead_count));
    }

    elements.push(json!({
        "tag": "note",
        "elements": [{"tag": "plain_text", "content": footer_parts.join(" · ")}],
    }));

    if dead_count > 0 {
        elements.push(json!({
            "tag": "note",
            "elements": [{"tag": "plain_text",
                "content": format!("无匹配 ({dead_count} 个): {dead_text}")}],
        }));
    }

    elements
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_matches_python_algorithm() {
        // 算法对照：HMAC-SHA256(key="{ts}\n{secret}", msg="") + base64
        // 手工构造已知向量（与 Python 相同算法）
        let sign = FeishuNotifier::gen_sign("1700000000", "mysecret");
        // golden 由 Python 生成：
        // base64.b64encode(hmac.new(b'1700000000\nmysecret', digestmod=hashlib.sha256).digest())
        assert_eq!(sign, "Jp33/xXhCipDEpjyHvEyc7mRSyXWHbNz6J8+C3qQKNo=");
    }

    #[test]
    fn card_elements_structure() {
        let stats = ReseedStats {
            total_torrents: 100,
            duplicate_count: 10,
            succeeded_count: 5,
            matched_count: 8,
            site_details: [("MTeam".into(), 3)].into(),
            ..Default::default()
        };
        let elements = build_card_elements(&stats);
        assert!(elements.len() >= 3);
        // 第一段是总览 markdown
        assert_eq!(elements[0]["tag"], "markdown");
        let content = elements[0]["content"].as_str().unwrap();
        assert!(content.contains("辅种成功"));
        assert!(content.contains("**5**"));
    }
}
