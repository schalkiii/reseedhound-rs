//! ReseedHound RS GUI：GPUI 仪表盘 + 首启向导 + 设置页 + Windows 托盘 + 后台调度。
//!
//! 架构：
//! - 主线程：GPUI Application（事件循环），根视图 RootView 二态（向导/仪表盘）
//! - 独立线程：托盘消息泵（tray-icon Windows 后端）
//! - 后台线程：tokio runtime 跑 ReseedScheduler，状态经 Arc<Mutex> 快照同步到 GUI
//!
//! 启动流程：
//! 1. 锚定 exe 目录（配置/db/logs 相对路径统一锚点）
//! 2. 无 config.yaml/sites.yaml → SetupWizard 引导配置（**不退出**）
//! 3. 向导完成 → 写盘 → 发 WizardDone 事件 → RootView 热切仪表盘 + 拉起调度线程
//! 4. 有配置 → 直接进仪表盘

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config_store;
mod dashboard;
mod settings;
mod setup_wizard;
mod text_input;
mod theme;
mod tray;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::*;
use sh_core::config::Config;

use config_store::ConfigStore;
use dashboard::{Dashboard, RunState};
use setup_wizard::SetupWizard;
use tray::TrayCommand;

/// 窗口根视图：向导 / 仪表盘二态（向导完成事件驱动切换）。
enum RootView {
    Wizard(Entity<SetupWizard>),
    Dash(Entity<Dashboard>),
}

/// 仪表盘构建材料（RootView 切换与首建共用）。
#[derive(Clone)]
struct DashParts {
    state: Arc<Mutex<RunState>>,
    trigger: tokio::sync::mpsc::Sender<()>,
    actions: tokio::sync::mpsc::Sender<dashboard::GuiAction>,
    tray: std::sync::mpsc::Sender<TrayCommand>,
    store: Arc<ConfigStore>,
}

/// 安装 panic 钩子：把 panic 位置与原因写进 `logs/panic.log`（追加，独立于 tracing，
/// 避免 panic 发生在日志锁内时二次死锁）。
///
/// GUI 子系统下没有控制台，未安装钩子时崩溃只留下 WER `0xc0000409`，无从定位。
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "?".to_string());
        let thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_string();
        let message = if let Some(text) = info.payload().downcast_ref::<&str>() {
            (*text).to_string()
        } else if let Some(text) = info.payload().downcast_ref::<String>() {
            text.clone()
        } else {
            "非字符串 panic payload".to_string()
        };
        let line = format!(
            "{} [PANIC] {location} thread={thread} {message}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        );
        eprintln!("{line}");
        let _ = std::fs::create_dir_all("logs");
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("logs/panic.log")
        {
            let _ = std::io::Write::write_all(&mut file, line.as_bytes());
            let _ = std::io::Write::flush(&mut file);
        }
    }));
}

impl RootView {
    fn build_dash(&mut self, parts: &DashParts, cx: &mut Context<Self>) {
        let dash = cx.new(|_| Dashboard {
            state: parts.state.clone(),
            trigger: parts.trigger.clone(),
            actions: parts.actions.clone(),
            tray: parts.tray.clone(),
            page: dashboard::Page::Dashboard,
            settings: None,
            config_store: parts.store.clone(),
            picker_open: false,
            site_names: Vec::new(),
            log_scroll: gpui::ScrollHandle::new(),
            last_log_len: 0,
        });
        *self = RootView::Dash(dash);
        cx.notify();
    }
}

impl Render for RootView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        match self {
            RootView::Wizard(w) => w.clone().into_any_element(),
            RootView::Dash(d) => d.clone().into_any_element(),
        }
    }
}

fn main() {
    install_panic_hook();

    // 双击运行时工作目录是 exe 所在目录，统一锚定到 exe 旁的 config.yaml / sites.yaml。
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let _ = std::env::set_current_dir(&exe_dir);
    // GUI 常驻运行：日志写文件（logs/reseed.log，随工作目录锚定 exe 旁），
    // keep_runs=0 不裁剪，远期清理由 weekly-report --clean 负责。
    // windows_subsystem 下控制台不可见，控制台层保留（RDP 调试时重定向可见）。
    sh_core::log_setup::setup_logging("INFO", "logs/reseed.log", 0);
    let config_path = exe_dir.join("config.yaml");
    let sites_path = exe_dir.join("sites.yaml");
    let config_path_str = config_path.to_string_lossy().into_owned();
    let sites_path_str = sites_path.to_string_lossy().into_owned();
    let store = Arc::new(ConfigStore::new(&config_path, &sites_path));

    // 共享状态 + 控制 channel
    let state = Arc::new(Mutex::new(RunState::default()));

    // 日志跟随线程：把引擎日志（脱敏后写盘的 logs/reseed.log）实时带进 GUI
    // 面板，并从进度行解析阶段 1/2/3 百分比（日志即数据源，显示口径一致）。
    {
        let st = state.clone();
        std::thread::spawn(move || follow_log_file("logs/reseed.log", &st));
    }
    let (stop_tx, _stop_rx) = tokio::sync::watch::channel(false);
    let (trigger_tx, trigger_rx) = tokio::sync::mpsc::channel::<()>(1);
    let (action_tx, action_rx) = tokio::sync::mpsc::channel::<dashboard::GuiAction>(1);
    // trigger_rx 单次移交（调度线程启动时取走）
    let trigger_rx_slot = Arc::new(Mutex::new(Some(trigger_rx)));

    // 启动调度（只允许一次；向导完成路径与已有配置路径互斥）
    // scheduler_handle：调度线程 JoinHandle，退出时 join 防止 tokio runtime 与
    // 引擎在进程 teardown 阶段继续访问已释放资源（P0：退出后后台线程仍跑数秒）。
    let (arm_scheduler, scheduler_handle) = {
        let armed = Arc::new(Mutex::new(false));
        let handle_slot = Arc::new(Mutex::new(None::<std::thread::JoinHandle<()>>));
        let cfg_path = config_path_str.clone();
        let sites_path = sites_path_str.clone();
        let st = state.clone();
        let stop = stop_tx.clone();
        let rx_slot = trigger_rx_slot.clone();
        let slot = handle_slot.clone();
        (
            Arc::new(move || {
                let mut flag = armed.lock().unwrap();
                if *flag {
                    return;
                }
                *flag = true;
                drop(flag);
                let config = match Config::load(&cfg_path, &sites_path) {
                    Ok(c) => Arc::new(c),
                    Err(e) => {
                        tracing::error!("配置加载失败，调度未启动: {e}");
                        return;
                    }
                };
                // 状态锁用 if let：另一线程 panic 导致锁中毒时不应再连锁 panic
                if let Ok(mut s) = st.lock() {
                    s.scheduler_on = config.scheduler_enabled();
                    s.mode = config.operation_mode();
                }
                let trigger_rx = rx_slot.lock().unwrap().take();
                let handle = spawn_scheduler(config, st.clone(), stop.clone(), trigger_rx);
                *slot.lock().unwrap() = Some(handle);
            }) as Arc<dyn Fn() + Send + Sync>,
            handle_slot,
        )
    };

    // 托盘（独立线程，始终常驻，与配置状态无关）
    let (tray_tx, tray_rx) = std::sync::mpsc::channel::<TrayCommand>();
    {
        let tray_trigger = trigger_tx.clone();
        let tray_stop = stop_tx.clone();
        std::thread::spawn(move || {
            if let Err(e) = tray::run_tray(tray_rx, tray_trigger, tray_stop) {
                tracing::error!("托盘异常: {e}");
            }
        });
    }

    // 工具动作执行线程（对齐 CLI 子命令：sync-cookies / check-dead / weekly-report）
    {
        let st = state.clone();
        let stop = stop_tx.clone();
        let cfg_path = config_path_str.clone();
        let sites_path = sites_path_str.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            let mut stop_rx = stop.subscribe();
            rt.block_on(async move {
                let mut action_rx = action_rx;
                loop {
                    let received = tokio::select! {
                        _ = stop_rx.changed() => None,
                        r = action_rx.recv() => r,
                    };
                    let Some(action) = received else { break };
                    if let Ok(mut s) = st.lock() {
                        s.action_running = true;
                    }
                    let result = run_gui_action(&action, &cfg_path, &sites_path).await;
                    if let Ok(mut s) = st.lock() {
                        s.action_running = false;
                        s.action_result = result;
                        s.push_log(format!("[工具] {action:?} 完成"));
                    }
                }
            });
        });
    }

    let has_config = config_path.exists() && sites_path.exists();

    Application::new().run(move |cx: &mut App| {
        text_input::bind_keys(cx);

        let quit_stop = stop_tx.clone();
        let quit_tray = tray_tx.clone();
        let quit_sched = scheduler_handle.clone();
        let _quit_subscription = cx.on_app_quit(move |_cx| {
            let _ = quit_stop.send(true);
            let _ = quit_tray.send(TrayCommand::Quit);
            // join 调度线程（等后台 runtime 的 select 收到 stop 后退出）。
            // 引擎一轮可能正扫几万种子/查站点，直接 join 会把 UI 线程一起冻死，
            // 用户只能强杀进程——改为后台 join + 有上限等待。
            if let Some(handle) = quit_sched.lock().unwrap().take() {
                let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
                std::thread::spawn(move || {
                    let _ = handle.join();
                    let _ = done_tx.send(());
                });
                if done_rx
                    .recv_timeout(Duration::from_secs(2))
                    .is_err()
                {
                    tracing::warn!("后台引擎本轮未结束，退出不再等待（本轮结果丢弃）");
                }
            }
            async move {}
        });

        // 已有配置：直接拉起调度
        if has_config {
            arm_scheduler();
        }

        let dash_parts = DashParts {
            state: state.clone(),
            trigger: trigger_tx.clone(),
            actions: action_tx.clone(),
            tray: tray_tx.clone(),
            store: store.clone(),
        };
        let arm = arm_scheduler.clone();

        // 窗口尺寸按主显示器收敛并居中：origin 默认 (0,0) 会让窗体贴死左上角，
        // 高 DPI/缩放时标题栏甚至落到屏幕外。
        let window_size = {
            const PREFERRED: (f32, f32) = (960.0, 680.0);
            match cx.primary_display() {
                Some(display) => {
                    let bounds = display.bounds();
                    let width: f32 = bounds.size.width.into();
                    let height: f32 = bounds.size.height.into();
                    size(
                        px((width - 80.0).clamp(720.0, PREFERRED.0)),
                        px((height - 120.0).clamp(520.0, PREFERRED.1)),
                    )
                }
                None => size(px(PREFERRED.0), px(PREFERRED.1)),
            }
        };
        let window_bounds = Bounds::centered(None, window_size, cx);

        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("ReseedHound RS".into()),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(window_bounds)),
                ..Default::default()
            },
            move |_window, cx| {
                let parts = dash_parts.clone();
                if has_config {
                    cx.new(|cx| {
                        let dash = cx.new(|_| Dashboard {
                            state: parts.state.clone(),
                            trigger: parts.trigger.clone(),
                            actions: parts.actions.clone(),
                            tray: parts.tray.clone(),
                            page: dashboard::Page::Dashboard,
                            settings: None,
                            config_store: parts.store.clone(),
                            picker_open: false,
                            site_names: Vec::new(),
                            log_scroll: gpui::ScrollHandle::new(),
                            last_log_len: 0,
                        });
                        RootView::Dash(dash)
                    })
                } else {
                    cx.new(|cx| {
                        // 向导：完成时发事件；RootView 订阅后切仪表盘 + 拉起调度
                        let wizard = cx.new(|cx| {
                            SetupWizard::new(
                                store.clone(),
                                Box::new(|_w, cx: &mut Context<SetupWizard>| {
                                    cx.emit(setup_wizard::WizardDone);
                                }),
                                cx,
                            )
                        });
                        cx.subscribe(&wizard, move |this: &mut RootView, _wiz, _evt: &setup_wizard::WizardDone, cx| {
                            arm();
                            this.build_dash(&parts, cx);
                        })
                        .detach();
                        RootView::Wizard(wizard)
                    })
                }
            },
        )
        .expect("open window");

        // 定时刷新（500ms 轮询共享状态）
        cx.spawn(async move |cx: &mut AsyncApp| {
            loop {
                if cx.update(|cx| cx.refresh_windows()).is_err() {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
            }
        })
        .detach();
    });
}

/// 执行 GUI 工具动作（对齐 CLI 子命令行为；结果文本回显到仪表盘）。
async fn run_gui_action(
    action: &dashboard::GuiAction,
    config_path: &str,
    sites_path: &str,
) -> String {
    match action {
        dashboard::GuiAction::SyncCookies => {
            // 参数缺省读 config.yaml（对齐 CLI sync-cookies 的缺省行为）
            match Config::load(config_path, sites_path) {
                Ok(config) => {
                    let url = config.cookiecloud_url();
                    let Some(uuid) = config.cookiecloud_uuid() else {
                        return "✗ CookieCloud 未配置（请在 设置 → 通知与集成 填写 UUID）".into();
                    };
                    let password = config.cookiecloud_password().unwrap_or_default();
                    match sh_engine::cookiecloud::sync_cookies(&url, &uuid, &password, sites_path).await {
                        Ok(n) => format!("✓ CookieCloud 同步完成：更新 {n} 个站点 Cookie"),
                        Err(e) => format!("✗ CookieCloud 同步失败: {e}"),
                    }
                }
                Err(e) => format!("✗ 配置加载失败: {e}"),
            }
        }
        dashboard::GuiAction::CheckDead => {
            match Config::load(config_path, sites_path) {
                Ok(config) => {
                    let url = config.savept_url();
                    match sh_engine::savept::fetch_dead_sites(&url, config.savept_timeout(), 5).await {
                        Ok(dead) => {
                            let local = sh_engine::savept::load_local_sites(sites_path, true)
                                .unwrap_or_default();
                            let matches = sh_engine::savept::match_local(&dead, &local);
                            sh_engine::savept::format_report(&matches, local.len())
                        }
                        Err(e) => format!("✗ 死亡名单拉取失败: {e}"),
                    }
                }
                Err(e) => format!("✗ 配置加载失败: {e}"),
            }
        }
        dashboard::GuiAction::WeeklyReport => {
            // 日志路径取 config.yaml log.file（缺省 logs/reseed.log），窗口 7 天
            let log_file = Config::load(config_path, sites_path)
                .ok()
                .and_then(|c| {
                    let f = c.log_file();
                    if f.is_empty() { None } else { Some(f) }
                })
                .unwrap_or_else(|| "logs/reseed.log".to_string());
            match sh_engine::weekly_report::window_start(&log_file, 7) {
                Ok(since) => {
                    let host_map = sh_engine::weekly_report::load_site_host_map(sites_path);
                    match sh_engine::weekly_report::parse_log(&log_file, since, &host_map) {
                        Ok((per_site, runs)) => {
                            sh_engine::weekly_report::generate_report(&per_site, &runs, since)
                        }
                        Err(e) => format!("✗ 周报生成失败: {e}"),
                    }
                }
                Err(e) => format!("✗ 日志读取失败: {e}"),
            }
        }
    }
}

/// 拉起后台调度线程（tokio runtime + ReseedScheduler 三路 select）。
/// 返回 JoinHandle 供退出时 join（防止 runtime 在进程 teardown 阶段访问已释放资源）。
fn spawn_scheduler(
    config: Arc<Config>,
    state: Arc<Mutex<RunState>>,
    stop_tx: tokio::sync::watch::Sender<bool>,
    trigger_rx: Option<tokio::sync::mpsc::Receiver<()>>,
) -> std::thread::JoinHandle<()> {
    use sh_engine::scheduler::ReseedScheduler;
    use sh_engine::seeder::RunOptions;

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let stop_rx = stop_tx.subscribe();
        rt.block_on(async move {            let on_start = {
                let bg_state = state.clone();
                Arc::new(move |cycle: u64| {
                    if let Ok(mut s) = bg_state.lock() {
                        s.running = true;
                        // 每轮开始重置终止标志（上一轮的终止请求不带到下一轮）
                        s.cancel_flag()
                            .store(false, std::sync::atomic::Ordering::Relaxed);
                        s.progress_label.clear();
                        s.progress_done = 0;
                        s.progress_total = 0;
                        s.push_log(format!("[{cycle}] 开始辅种…"));
                    }
                }) as Arc<dyn Fn(u64) + Send + Sync>
            };
            let on_report = {
                let bg_state = state.clone();
                Arc::new(move |report: &sh_engine::scheduler::CycleReport| {
                    let Ok(mut s) = bg_state.lock() else { return };
                    s.running = false;
                    s.cycle = report.cycle;
                    s.total_torrents = report.total_torrents;
                    s.succeeded_count = report.succeeded;
                    s.failed_count = report.failed;
                    s.matched_count = report.matched;
                    s.dead_count = report.dead;
                    s.sites = report.sites.clone();
                    s.last_run = format!(
                        "{} · 成功 {} / 失败 {} / 耗时 {:.1}s",
                        report.finished_at, report.succeeded, report.failed,
                        report.duration_secs
                    );
                    if let Some(e) = &report.error {
                        s.push_log(e.clone());
                    }
                    // 站点范围只作用于一轮：跑完清空，定时轮次恢复全部站点
                    if !s.selected_sites.is_empty() {
                        s.selected_sites.clear();
                    }
                }) as Arc<dyn Fn(&sh_engine::scheduler::CycleReport) + Send + Sync>
            };

            let mut scheduler = ReseedScheduler::new(config, RunOptions::default(), stop_rx)
                .with_on_report(on_report)
                .with_on_cycle_start(on_start);
            if let Some(rx) = trigger_rx {
                scheduler = scheduler.with_trigger(rx);
            }
            // 演练模式等运行选项以共享状态为准（GUI 可实时切换，每轮生效）
            {
                let state_ref = state.clone();
                scheduler = scheduler.with_options_provider(Arc::new(move || {
                    state_ref.lock().ok().map(|s| RunOptions {
                        dry_run: s.dry_run,
                        // GUI「站点范围」选择：非空则本轮只跑这些站点（对应 CLI --site）
                        site_name: if s.selected_sites.is_empty() {
                            None
                        } else {
                            Some(s.selected_sites.join(","))
                        },
                        // 「终止本轮」标志：每轮 on_cycle_start 重置
                        cancel: s.cancel.clone(),
                    })
                }));
            }

            // 调度开关/间隔由 run() 内部每轮（及等待期间每 5s）从磁盘重读，
            // GUI 修改保存后实时生效，无需重启程序
            if let Err(e) = scheduler.run().await {
                tracing::error!("调度器异常: {e}");
            }
        });
    })
}

/// 跟随日志文件：新行推入 GUI 日志面板，并解析阶段进度。
///
/// 日志即数据源——面板显示与文件记录口径一致（含脱敏），不额外接 tracing 管线。
fn follow_log_file(path: &str, state: &Arc<Mutex<RunState>>) {
    use std::io::{Read, Seek, SeekFrom};
    // 从当前文件尾开始跟（历史行不补，面板随新轮次自然填充）
    let mut offset = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    loop {
        std::thread::sleep(std::time::Duration::from_millis(400));
        let Ok(mut f) = std::fs::OpenOptions::new().read(true).open(path) else {
            continue;
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < offset {
            // 日志被裁剪/轮转：从头跟
            offset = 0;
        }
        if len == offset {
            continue;
        }
        if f.seek(SeekFrom::Start(offset)).is_err() {
            continue;
        }
        let mut buf = String::new();
        if f.read_to_string(&mut buf).is_err() {
            continue;
        }
        offset = len;
        for line in buf.lines() {
            if line.trim().is_empty() {
                continue;
            }
            apply_progress_line(state, line);
            if let Ok(mut s) = state.lock() {
                s.push_log(line.to_string());
            }
        }
    }
}

/// 从进度日志行解析 (标签, 完成, 总数)。行样例：
/// `[阶段1] 解析进度 5000/41903（缓存命中 0）`、`[阶段2] 站点进度 1/65 · 站点名`、
/// `[阶段3] 进度 50/4248 颗种子`
fn apply_progress_line(state: &Arc<Mutex<RunState>>, line: &str) {
    const MARKERS: [(&str, &str); 3] = [
        ("[阶段1] 解析进度 ", "阶段1 解析"),
        ("[阶段2] 站点进度 ", "阶段2 查询"),
        ("[阶段3] 进度 ", "阶段3 添加"),
    ];
    for (marker, label) in MARKERS {
        let Some(pos) = line.find(marker) else {
            continue;
        };
        let rest = &line[pos + marker.len()..];
        let mut parts = rest.split('/');
        let (Some(done_s), Some(total_s)) = (parts.next(), parts.next()) else {
            continue;
        };
        let done = leading_u64(done_s);
        let total = leading_u64(total_s);
        if total > 0 {
            if let Ok(mut s) = state.lock() {
                s.progress_label = label.to_string();
                s.progress_done = done;
                s.progress_total = total;
            }
        }
        return;
    }
}

/// 取字符串前导数字（进度行尾部带「（缓存命中 N）」「颗种子」等后缀）。
fn leading_u64(s: &str) -> u64 {
    s.chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}
