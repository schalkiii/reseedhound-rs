//! ReseedHound RS 命令行入口。
//!
//! 对齐 Python run.py 的命令面：
//! - reseed        单次辅种（scheduler.enabled=true 时自动切换定时）
//! - schedule      定时调度
//! - sync-cookies  CookieCloud 同步（M3：见 cookie 模块）
//! - check-dead    PT 站点存活性检测
//! - gui           启动 GPUI 图形界面 + 托盘（默认，或 SEEDHOUND_MODE=gui）
//!
//! 顶层缺省值与 Python CLI_DEFAULTS 对齐；业务缺省在 sh-core::config::defaults。

use std::sync::Arc;

use clap::{Parser, Subcommand};
use sh_core::config::Config;
use sh_engine::feishu::FeishuNotifier;
use sh_engine::seeder::{ReseedEngine, RunOptions};
use sh_engine::scheduler::ReseedScheduler;

#[derive(Parser, Debug)]
#[command(name = "reseedhound", about = "ReseedHound RS - PT 辅种引擎")]
struct Cli {
    /// 演练模式：查询站点但不添加种子
    #[arg(long, global = true)]
    dry_run: bool,

    /// 跳过飞书通知推送
    #[arg(long, global = true)]
    no_feishu: bool,

    /// 指定站点名称（逗号分隔）
    #[arg(long, global = true)]
    site: Option<String>,

    /// 辅种模式
    #[arg(long, global = true)]
    mode: Option<String>,

    /// 主配置文件路径
    #[arg(long, global = true, default_value = "config.yaml")]
    config: String,

    /// sites 配置路径
    #[arg(long, global = true, default_value = "sites.yaml")]
    sites_path: String,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug, Clone)]
enum Command {
    /// 单次辅种
    Reseed,
    /// 定时调度
    Schedule,
    /// 对照 savept.icu 死亡名单，推荐禁用已死亡站点
    CheckDead {
        /// 把命中的站点写入 sites.yaml（enabled: false）；缺省只打印建议不改配置
        #[arg(long)]
        apply: bool,
        /// savept 数据源地址（缺省读取 config.yaml 的 savept.url）
        #[arg(long)]
        savept_url: Option<String>,
    },
    /// 从 CookieCloud 同步站点 Cookie
    SyncCookies {
        /// CookieCloud 服务地址（缺省读取 config.yaml 的 cookiecloud.url）
        #[arg(long)]
        cookiecloud_url: Option<String>,
        /// CookieCloud UUID（缺省读取 config.yaml 的 cookiecloud.uuid）
        #[arg(long)]
        cookiecloud_uuid: Option<String>,
        /// CookieCloud 解密密码（缺省读取 config.yaml 的 cookiecloud.password）
        #[arg(long)]
        cookiecloud_password: Option<String>,
    },
    /// 日志周报生成 + 远期日志清理（对齐 scripts/weekly_report.py）
    WeeklyReport {
        /// 日志文件路径
        #[arg(long, default_value = "logs/reseed.log")]
        log: String,
        /// sites.yaml 路径（失败 URL 归因到站点）
        #[arg(long, default_value = "sites.yaml")]
        sites: String,
        /// 统计窗口（天）
        #[arg(long, default_value_t = 7)]
        days: i64,
        /// 将周报写入该 Markdown 文件（缺省打印到控制台）
        #[arg(long)]
        markdown: Option<String>,
        /// 执行日志清理（默认仅生成周报）
        #[arg(long)]
        clean: bool,
        /// 清理时保留最近天数
        #[arg(long, default_value_t = 30)]
        keep_days: i64,
        /// 清理时至少保留的轮次（安全下限）
        #[arg(long, default_value_t = 3)]
        keep_runs: usize,
        /// 真正截断日志（默认仅预览）
        #[arg(long)]
        apply: bool,
    },
    /// 启动图形界面 + 系统托盘
    Gui,
}

fn load_config(cli: &Cli) -> anyhow::Result<Arc<Config>> {
    let mut config = Config::load(&cli.config, &cli.sites_path)?;
    // --mode 临时覆盖运行模式（对齐 Python run.py 的 setdefault 注入，不写回文件）
    if let Some(mode) = &cli.mode {
        anyhow::ensure!(
            mode == "pieces_hash" || mode == "jackett" || mode == "both",
            "无效的辅种模式 {mode}（可选: pieces_hash / jackett / both）"
        );
        config.override_mode(mode);
    }
    Ok(Arc::new(config))
}

fn run_options(cli: &Cli) -> RunOptions {
    RunOptions {
        dry_run: cli.dry_run,
        site_name: cli.site.clone(),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // 容器场景：未显式指定子命令时，按 SEEDHOUND_MODE 环境变量选择运行模式
    // （Dockerfile 默认 reseed；可用 -e SEEDHOUND_MODE=schedule 切换为定时运行）
    let command = match cli.command.clone() {
        Some(cmd) => cmd,
        None => match std::env::var("SEEDHOUND_MODE").ok().as_deref() {
            Some("reseed") => Command::Reseed,
            Some("schedule") => Command::Schedule,
            Some("check-dead") => Command::CheckDead {
                apply: false,
                savept_url: None,
            },
            Some("sync-cookies") => Command::SyncCookies {
                cookiecloud_url: None,
                cookiecloud_uuid: None,
                cookiecloud_password: None,
            },
            Some("gui") => Command::Gui,
            Some(other) => {
                eprintln!("未知 SEEDHOUND_MODE={other}，回退为 reseed");
                Command::Reseed
            }
            None => Command::Gui,
        },
    };

    // 日志初始化需要先读配置（level/file）；sync-cookies / check-dead /
    // weekly-report 不依赖配置文件也应有日志，按缺省初始化。
    match &command {
        Command::SyncCookies { .. }
        | Command::CheckDead { .. }
        | Command::Gui
        | Command::WeeklyReport { .. } => {
            sh_core::log_setup::setup_logging("INFO", "logs/reseed.log", 0);
        }
        Command::Reseed | Command::Schedule => {
            let config = load_config(&cli)?;
            // scheduler.enabled=true 时，显式 reseed 自动切换为定时运行
            // （对齐 Python run.py，sync-cookies 不受影响）
            let upgraded = matches!(command, Command::Reseed) && config.scheduler_enabled();
            if upgraded {
                tracing::info!("scheduler.enabled=true，自动切换为定时运行模式");
            }
            let run_scheduled =
                matches!(command, Command::Schedule) || matches!(command, Command::Reseed) && config.scheduler_enabled();
            // 定时模式长驻，不裁剪历史日志；一次性运行仅保留最近 3 次
            let keep_runs: i64 = if run_scheduled { 0 } else { 3 };
            sh_core::log_setup::setup_logging(
                &config.log_level(),
                &config.log_file(),
                keep_runs,
            );
            return dispatch_run(&cli, command, run_scheduled, config).await;
        }
    }
    dispatch(&cli, command).await
}

/// reseed / schedule（含自动升级）：统一走调度器或单次运行。
async fn dispatch_run(
    cli: &Cli,
    command: Command,
    run_scheduled: bool,
    config: Arc<Config>,
) -> anyhow::Result<()> {
    if run_scheduled && matches!(command, Command::Schedule | Command::Reseed) {
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        // Ctrl+C 优雅退出
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            let _ = stop_tx.send(true);
        });
        let mut scheduler = ReseedScheduler::new(config, run_options(cli), stop_rx);
        scheduler.run().await?;
        return Ok(());
    }
    run_once(cli, config).await
}

/// 单次辅种 + 飞书推送（对齐 Python run_once：CLI 与调度器共用同一条路径）。
async fn run_once(cli: &Cli, config: Arc<Config>) -> anyhow::Result<()> {
    let engine = ReseedEngine::new(config.clone()).await?;
    engine.start().await?;
    let stats = engine.run(&run_options(cli)).await?;

    if !cli.no_feishu {
        if let Some(notifier) = FeishuNotifier::from_config(
            &config.feishu_webhook_url(),
            config.feishu_secret().as_deref(),
        ) {
            notifier.send_report_card(&stats).await;
            if !stats.credential_failures.is_empty() {
                notifier.send_credential_alert(&stats.credential_failures).await;
            }
        }
    }
    println!(
        "辅种完成: 扫描={}, 匹配={}, 成功={}, 失败={}, 耗时={}",
        stats.total_torrents,
        stats.matched_count,
        stats.succeeded_count,
        stats.failed_count,
        stats.duration_str()
    );
    Ok(())
}

/// sync-cookies / check-dead / gui：无需提前加载配置。
async fn dispatch(cli: &Cli, command: Command) -> anyhow::Result<()> {
    match command {
        Command::CheckDead { apply, savept_url } => cmd_check_dead(cli, apply, savept_url).await,
        Command::SyncCookies {
            cookiecloud_url,
            cookiecloud_uuid,
            cookiecloud_password,
        } => {
            cmd_sync_cookies(
                cli,
                cookiecloud_url,
                cookiecloud_uuid,
                cookiecloud_password,
            )
            .await
        }
        Command::WeeklyReport {
            log,
            sites,
            days,
            markdown,
            clean,
            keep_days,
            keep_runs,
            apply,
        } => {
            return cmd_weekly_report(
                &log,
                &sites,
                days,
                markdown.as_deref(),
                clean,
                keep_days,
                keep_runs,
                apply,
            )
            .await;
        }
        Command::Gui => {            // 双击 reseedhound.exe（无参数）时走这里：拉起 GUI 进程后本进程退出。
            // 不在 CLI 进程内直接跑 GPUI，避免 tokio 主线程与 GPUI 事件循环冲突。
            let exe_dir = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            let gui_exe = exe_dir.join("reseedhound-gui.exe");
            if !gui_exe.exists() {
                anyhow::bail!(
                    "未找到 GUI 程序：{}\n（CLI 发行版未附带 GUI，请使用 reseedhound-gui.exe）",
                    gui_exe.display()
                );
            }
            // 继承当前工作目录，GUI 侧自行锚定 exe 旁的配置文件。
            std::process::Command::new(&gui_exe).spawn()?;
            println!("已启动图形界面：{}", gui_exe.display());
            Ok(())
        }
        // reseed / schedule 已在 dispatch_run 处理，不会落到这里
        Command::Reseed | Command::Schedule => unreachable!("handled in dispatch_run"),
    }
}

/// 日志周报 + 清理（对齐 Python scripts/weekly_report.py main）。
#[allow(clippy::too_many_arguments)]
async fn cmd_weekly_report(
    log_path: &str,
    sites_path: &str,
    days: i64,
    markdown_path: Option<&str>,
    clean: bool,
    keep_days: i64,
    keep_runs: usize,
    apply: bool,
) -> anyhow::Result<()> {
    use sh_engine::weekly_report;

    if !std::path::Path::new(log_path).exists() {
        anyhow::bail!("日志文件不存在: {log_path}");
    }

    let host_map = weekly_report::load_site_host_map(sites_path);
    // 以日志中最新时间戳为参考终点
    let since = weekly_report::window_start(log_path, days)?;
    let (per_site, runs) = weekly_report::parse_log(log_path, since, &host_map)?;
    let report = weekly_report::generate_report(&per_site, &runs, since);

    if let Some(path) = markdown_path {
        std::fs::write(path, &report)?;
        println!("周报已写入 {path}");
    } else {
        println!("{report}");
    }

    if clean {
        println!(
            "{}",
            weekly_report::cleanup_log(log_path, keep_days, keep_runs, apply)?
        );
    }
    Ok(())
}

/// 从 CookieCloud 同步站点 Cookie（凭据只从本地 config.yaml 读取，CLI 可覆盖）。
async fn cmd_sync_cookies(
    cli: &Cli,
    url_override: Option<String>,
    uuid_override: Option<String>,
    password_override: Option<String>,
) -> anyhow::Result<()> {
    use sh_engine::cookiecloud;

    let config = load_config(cli)?;
    let url = url_override.unwrap_or_else(|| config.cookiecloud_url());
    let uuid = uuid_override.or_else(|| config.cookiecloud_uuid());
    let password = password_override.or_else(|| config.cookiecloud_password());

    let (Some(uuid), Some(password)) = (uuid, password) else {
        anyhow::bail!(
            "CookieCloud 未配置：请在 {} 的 cookiecloud 块填写 uuid 与 password",
            cli.config
        );
    };
    match cookiecloud::sync_cookies(&url, &uuid, &password, &cli.sites_path).await {
        Ok(n) => {
            println!("同步完成: {n} 个站点已更新 Cookie");
            Ok(())
        }
        Err(e) => {
            println!("同步失败: {e}");
            std::process::exit(1);
        }
    }
}
async fn cmd_check_dead(
    cli: &Cli,
    apply: bool,
    savept_url: Option<String>,
) -> anyhow::Result<()> {
    use sh_engine::savept;

    let config = load_config(cli)?;
    let base_url = savept_url.unwrap_or_else(|| config.savept_url());
    let timeout = config.savept_timeout();

    let local_sites = savept::load_local_sites(&cli.sites_path, true)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    tracing::info!(
        "拉取 savept.icu 死亡名单并对照 {} 个启用中的本地站点...",
        local_sites.len()
    );
    let dead_sites = savept::fetch_dead_sites(&base_url, timeout, 50).await?;
    let matches = savept::match_local(&dead_sites, &local_sites);

    let report = savept::format_report(&matches, local_sites.len());
    println!();
    println!("{report}");

    if apply && !matches.is_empty() {
        let names: Vec<String> = matches.iter().map(|m| m.local_name.clone()).collect();
        match savept::disable_sites_in_yaml(&cli.sites_path, &names) {
            Ok(changed) if !changed.is_empty() => {
                tracing::info!("已禁用 {} 个站点: {}", changed.len(), changed.join("、"));
            }
            Ok(_) => {
                tracing::warn!("没有站点被写入 enabled: false");
            }
            Err(e) => return Err(anyhow::anyhow!("{e}")),
        }
    } else if !matches.is_empty() {
        tracing::info!("以上仅为建议；确认后加 --apply 写入 sites.yaml");
    }
    Ok(())
}
