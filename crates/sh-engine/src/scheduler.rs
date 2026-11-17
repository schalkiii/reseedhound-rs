//! 定时调度器：循环运行辅种引擎。
//!
//! 对齐 Python src/scheduler.py：
//! - 每轮重建引擎保证状态干净
//! - 间隔 = scheduler.interval_minutes * 60
//! - 启动延迟 startup_delay / run_on_start 语义
//! - Ctrl+C / 托盘退出 → 优雅停止（本轮结束后退出）

use std::sync::Arc;
use std::time::Duration;

use sh_core::config::Config;

use crate::feishu::FeishuNotifier;
use crate::seeder::{ReseedEngine, RunOptions};

/// 每轮结果回调类型（GUI 状态同步）。
pub type CycleReportCallback = Arc<dyn Fn(&CycleReport) + Send + Sync>;

/// 每轮开始回调类型（GUI 运行状态同步）。
pub type CycleStartCallback = Arc<dyn Fn(u64) + Send + Sync>;

pub struct ReseedScheduler {
    config: Arc<Config>,
    /// config.yaml 路径（CookieCloud 同步后重建 Config 用）
    config_path: String,
    run_options: RunOptions,
    stop: tokio::sync::watch::Receiver<bool>,
    trigger: Option<tokio::sync::mpsc::Receiver<()>>,
    on_report: Option<CycleReportCallback>,
    on_cycle_start: Option<CycleStartCallback>,
}

/// 单站点轮次摘要。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SiteCycleSummary {
    pub name: String,
    pub matched: u64,
    pub succeeded: u64,
    pub failed: u64,
}

/// 调度器运行摘要（GUI 上报用）。
#[derive(Debug, Clone, Default)]
pub struct CycleReport {
    pub cycle: u64,
    pub total_torrents: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub matched: u64,
    pub dead: u64,
    /// 本轮耗时（秒）。
    pub duration_secs: f64,
    /// 本轮结束时刻（本地时间）。
    pub finished_at: String,
    /// 站点明细（按名称排序）。
    pub sites: Vec<SiteCycleSummary>,
    pub error: Option<String>,
}

impl ReseedScheduler {
    pub fn new(
        config: Arc<Config>,
        run_options: RunOptions,
        stop: tokio::sync::watch::Receiver<bool>,
    ) -> Self {
        Self {
            config_path: "config.yaml".to_string(),
            config,
            run_options,
            stop,
            trigger: None,
            on_report: None,
            on_cycle_start: None,
        }
    }

    /// 指定 config.yaml 路径（CookieCloud 同步后重建 Config 用）。
    pub fn with_config_path(mut self, path: &str) -> Self {
        self.config_path = path.to_string();
        self
    }

    /// 每轮前置：从 CookieCloud 同步站点 Cookie（受 cookiecloud.enabled 控制）。
    ///
    /// 同步失败仅告警并继续本轮，避免 CookieCloud 不可达导致整轮辅种失败。
    /// 同步成功后从磁盘重建 Config（等价 Python reload_sites），本轮引擎使用最新 Cookie。
    async fn sync_cookies_if_enabled(&self) -> Arc<Config> {
        if !self.config.cookiecloud_enabled() {
            return self.config.clone();
        }
        let (Some(uuid), Some(password)) = (
            self.config.cookiecloud_uuid(),
            self.config.cookiecloud_password(),
        ) else {
            tracing::warn!("CookieCloud 未配置 uuid/password，跳过本轮 Cookie 同步");
            return self.config.clone();
        };
        let sites_path = self.config.sites_path.to_string_lossy().to_string();
        match crate::cookiecloud::sync_cookies(
            &self.config.cookiecloud_url(),
            &uuid,
            &password,
            &sites_path,
        )
        .await
        {
            Ok(_) => match Config::load(&self.config_path, &sites_path) {
                Ok(fresh) => {
                    tracing::info!("CookieCloud 同步完成，已重新加载 sites 配置");
                    Arc::new(fresh)
                }
                Err(e) => {
                    tracing::warn!("sites 配置重载失败(本轮使用已有 Cookie): {e}");
                    self.config.clone()
                }
            },
            Err(e) => {
                tracing::warn!("CookieCloud 同步异常(本轮使用已有 Cookie): {e}");
                self.config.clone()
            }
        }
    }

    /// 注入手动触发通道（托盘"立即辅种"等场景）。
    pub fn with_trigger(mut self, rx: tokio::sync::mpsc::Receiver<()>) -> Self {
        self.trigger = Some(rx);
        self
    }

    /// 注入每轮结果回调（GUI 状态同步）。
    pub fn with_on_report(mut self, f: CycleReportCallback) -> Self {
        self.on_report = Some(f);
        self
    }

    /// 注入每轮开始回调（GUI 运行状态同步）。
    pub fn with_on_cycle_start(mut self, f: CycleStartCallback) -> Self {
        self.on_cycle_start = Some(f);
        self
    }

    /// 是否启用定时调度。
    pub fn scheduler_enabled(&self) -> bool {
        self.config.scheduler_enabled()
    }

    /// 取出手动触发通道（禁用定时调度时常驻循环自用）。
    pub fn take_trigger(&mut self) -> Option<tokio::sync::mpsc::Receiver<()>> {
        self.trigger.take()
    }

    /// 对外触发结果回调（GUI 状态同步）。
    pub fn report_to(&self, report: &CycleReport) {
        if let Some(cb) = &self.on_report {
            cb(report);
        }
    }

    /// 单轮辅种（CookieCloud 前置同步 + 引擎重建 + 飞书通知）。
    pub async fn run_cycle(&self, cycle: u64) -> CycleReport {
        let start = std::time::Instant::now();
        if let Some(cb) = &self.on_cycle_start {
            cb(cycle);
        }
        let mut report = CycleReport {
            cycle,
            ..Default::default()
        };
        let config = self.sync_cookies_if_enabled().await;
        match ReseedEngine::new(config).await {
            Ok(engine) => {
                if let Err(e) = engine.start().await {
                    report.error = Some(format!("引擎启动失败: {e}"));
                    return report;
                }
                match engine.run(&self.run_options).await {
                    Ok(stats) => {
                        report.total_torrents = stats.total_torrents;
                        report.succeeded = stats.succeeded_count;
                        report.failed = stats.failed_count;
                        report.matched = stats.matched_count;
                        report.dead = stats.dead_count;
                        // 站点明细（名称排序）
                        let mut names: std::collections::BTreeSet<String> =
                            stats.site_match_counts.keys().cloned().collect();
                        names.extend(stats.site_succeeded.keys().cloned());
                        names.extend(stats.site_failed.keys().cloned());
                        report.sites = names
                            .into_iter()
                            .map(|name| SiteCycleSummary {
                                matched: stats.site_match_counts.get(&name).copied().unwrap_or(0),
                                succeeded: stats.site_succeeded.get(&name).copied().unwrap_or(0),
                                failed: stats.site_failed.get(&name).copied().unwrap_or(0),
                                name,
                            })
                            .collect();
                        // 飞书通知（凭据始终取原始配置，与同步无关）
                        if let Some(notifier) = FeishuNotifier::from_config(
                            &self.config.feishu_webhook_url(),
                            self.config.feishu_secret().as_deref(),
                        ) {
                            notifier.send_report_card(&stats).await;
                            if !stats.credential_failures.is_empty() {
                                notifier
                                    .send_credential_alert(&stats.credential_failures)
                                    .await;
                            }
                        }
                    }
                    Err(e) => {
                        report.error = Some(format!("辅种运行失败: {e}"));
                    }
                }
            }
            Err(e) => {
                report.error = Some(format!("引擎构建失败: {e}"));
            }
        }
        report.duration_secs = start.elapsed().as_secs_f64();
        report.finished_at = chrono::Local::now().format("%H:%M:%S").to_string();
        report
    }

    /// 调度主循环：定时轮次 + 手动触发 + 退出信号三路 select。
    pub async fn run(&mut self) -> anyhow::Result<()> {
        let interval = Duration::from_secs_f64(self.config.scheduler_interval_seconds());
        let startup_delay = Duration::from_secs_f64(self.config.scheduler_startup_delay());
        let run_on_start = self.config.scheduler_run_on_start();

        let mut cycle: u64 = 0;
        tracing::info!(
            "调度器启动: 间隔 {:.0?}{}",
            interval,
            if run_on_start {
                ", 立即执行首轮"
            } else {
                ""
            }
        );

        if startup_delay > Duration::ZERO {
            tracing::info!("启动延迟 {:.0?}", startup_delay);
            tokio::select! {
                _ = tokio::time::sleep(startup_delay) => {}
                _ = self.stop.changed() => {
                    tracing::info!("退出信号：调度器未启动即停止");
                    return Ok(());
                }
            }
        }

        loop {
            if run_on_start || cycle > 0 {
                cycle += 1;
                self.execute_cycle(cycle).await;
            } else {
                cycle += 1;
            }

            // 等待间隔 / 手动触发 / 退出信号
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = self.stop.changed() => {
                    tracing::info!("收到退出信号，调度器停止");
                    return Ok(());
                }
                received = async {
                    match self.trigger.as_mut() {
                        Some(rx) => rx.recv().await.is_some(),
                        None => std::future::pending().await,
                    }
                } => {
                    if received {
                        cycle += 1;
                        tracing::info!("===== 手动触发 第 {cycle} 轮辅种 =====");
                        self.execute_cycle(cycle).await;
                    } else {
                        // 触发通道关闭（发送端全部 drop）→ 忽略，继续定时轮次
                    }
                }
            }
        }
    }

    /// 执行单轮并上报（日志 + 回调）。
    async fn execute_cycle(&self, cycle: u64) {
        tracing::info!("===== 第 {cycle} 轮辅种 =====");
        let report = self.run_cycle(cycle).await;
        if let Some(err) = &report.error {
            tracing::error!("{err}");
        }
        if let Some(cb) = &self.on_report {
            cb(&report);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scheduler_stops_on_signal() {
        let cfg_path = std::env::temp_dir().join("sh_sched_test_cfg.yaml");
        let sites_path = std::env::temp_dir().join("sh_sched_test_sites.yaml");
        // run_on_start=false：避免首轮 execute_cycle 构建引擎时
        // 以默认 db 路径在 crate 目录下创建 data/cache.db
        std::fs::write(
            &cfg_path,
            "global: {mode: pieces_hash}\nscheduler: {enabled: true, run_on_start: false}\n",
        )
        .unwrap();
        std::fs::write(&sites_path, "sites: []\n").unwrap();
        let config = Arc::new(
            Config::load(cfg_path.to_str().unwrap(), sites_path.to_str().unwrap()).unwrap(),
        );
        let (tx, rx) = tokio::sync::watch::channel(false);
        let mut scheduler = ReseedScheduler::new(config, RunOptions::default(), rx);

        // 立即发停止信号：run() 应快速返回而非进入循环
        tx.send(true).unwrap();
        let start = std::time::Instant::now();
        // run_on_start=false 时首轮直接等待 → 收到信号退出
        let result = tokio::time::timeout(Duration::from_secs(2), scheduler.run()).await;
        assert!(result.is_ok(), "scheduler should exit promptly");
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
