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
struct DashParts {
    state: Arc<Mutex<RunState>>,
    trigger: tokio::sync::mpsc::Sender<()>,
    tray: std::sync::mpsc::Sender<TrayCommand>,
    store: Arc<ConfigStore>,
}

impl RootView {
    fn build_dash(&mut self, parts: &DashParts, cx: &mut Context<Self>) {
        let dash = cx.new(|_| Dashboard {
            state: parts.state.clone(),
            trigger: parts.trigger.clone(),
            tray: parts.tray.clone(),
            page: dashboard::Page::Dashboard,
            settings: None,
            config_store: parts.store.clone(),
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
    // GUI 常驻运行：日志写文件（logs/reseed.log），keep_runs=0 不裁剪，
    // 远期清理由 weekly-report --clean 负责。windows_subsystem 下控制台不可见，
    // 控制台层保留（RDP 调试时重定向可见）。
    sh_core::log_setup::setup_logging("INFO", "logs/reseed.log", 0);

    // 双击运行时工作目录是 exe 所在目录，统一锚定到 exe 旁的 config.yaml / sites.yaml。
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let _ = std::env::set_current_dir(&exe_dir);
    let config_path = exe_dir.join("config.yaml");
    let sites_path = exe_dir.join("sites.yaml");
    let config_path_str = config_path.to_string_lossy().into_owned();
    let sites_path_str = sites_path.to_string_lossy().into_owned();
    let store = Arc::new(ConfigStore::new(&config_path, &sites_path));

    // 共享状态 + 控制 channel
    let state = Arc::new(Mutex::new(RunState::default()));
    let (stop_tx, _stop_rx) = tokio::sync::watch::channel(false);
    let (trigger_tx, trigger_rx) = tokio::sync::mpsc::channel::<()>(1);
    // trigger_rx 单次移交（调度线程启动时取走）
    let trigger_rx_slot = Arc::new(Mutex::new(Some(trigger_rx)));

    // 启动调度（只允许一次；向导完成路径与已有配置路径互斥）
    let arm_scheduler = {
        let armed = Arc::new(Mutex::new(false));
        let cfg_path = config_path_str.clone();
        let sites_path = sites_path_str.clone();
        let st = state.clone();
        let stop = stop_tx.clone();
        let rx_slot = trigger_rx_slot.clone();
        move || {
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
            let mut s = st.lock().unwrap();
            s.scheduler_on = config.scheduler_enabled();
            s.mode = config.operation_mode();
            drop(s);
            let trigger_rx = rx_slot.lock().unwrap().take();
            spawn_scheduler(config, st.clone(), stop.clone(), trigger_rx);
        }
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

    let has_config = config_path.exists() && sites_path.exists();

    Application::new().run(move |cx: &mut App| {
        text_input::bind_keys(cx);

        let quit_stop = stop_tx.clone();
        let quit_tray = tray_tx.clone();
        let _quit_subscription = cx.on_app_quit(move |_cx| {
            let _ = quit_stop.send(true);
            let _ = quit_tray.send(TrayCommand::Quit);
            async move {}
        });

        // 已有配置：直接拉起调度
        if has_config {
            arm_scheduler();
        }

        let dash_parts = DashParts {
            state: state.clone(),
            trigger: trigger_tx.clone(),
            tray: tray_tx.clone(),
            store: store.clone(),
        };
        let arm = arm_scheduler.clone();

        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("ReseedHound RS".into()),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    size: size(px(960.), px(680.)),
                    origin: Default::default(),
                })),
                ..Default::default()
            },
            move |_window, cx| {
                let parts = dash_parts.clone_dyn();
                if has_config {
                    cx.new(|cx| {
                        let dash = cx.new(|_| Dashboard {
                            state: parts.state.clone(),
                            trigger: parts.trigger.clone(),
                            tray: parts.tray.clone(),
                            page: dashboard::Page::Dashboard,
                            settings: None,
                            config_store: parts.store.clone(),
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

/// DashParts 跨闭包克隆（字段都是 Arc/Sender，均可 clone）。
impl DashParts {
    fn clone_dyn(&self) -> DashParts {
        DashParts {
            state: self.state.clone(),
            trigger: self.trigger.clone(),
            tray: self.tray.clone(),
            store: self.store.clone(),
        }
    }
}

/// 拉起后台调度线程（tokio runtime + ReseedScheduler 三路 select）。
fn spawn_scheduler(
    config: Arc<Config>,
    state: Arc<Mutex<RunState>>,
    stop_tx: tokio::sync::watch::Sender<bool>,
    trigger_rx: Option<tokio::sync::mpsc::Receiver<()>>,
) {
    use sh_engine::scheduler::ReseedScheduler;
    use sh_engine::seeder::RunOptions;

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let stop_rx = stop_tx.subscribe();
        rt.block_on(async move {
            let on_start = {
                let bg_state = state.clone();
                Arc::new(move |cycle: u64| {
                    let mut s = bg_state.lock().unwrap();
                    s.running = true;
                    s.push_log(format!("[{cycle}] 开始辅种…"));
                }) as Arc<dyn Fn(u64) + Send + Sync>
            };
            let on_report = {
                let bg_state = state.clone();
                Arc::new(move |report: &sh_engine::scheduler::CycleReport| {
                    let mut s = bg_state.lock().unwrap();
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
                }) as Arc<dyn Fn(&sh_engine::scheduler::CycleReport) + Send + Sync>
            };

            let mut scheduler = ReseedScheduler::new(config, RunOptions::default(), stop_rx)
                .with_on_report(on_report)
                .with_on_cycle_start(on_start);
            if let Some(rx) = trigger_rx {
                scheduler = scheduler.with_trigger(rx);
            }

            if scheduler.scheduler_enabled() {
                if let Err(e) = scheduler.run().await {
                    tracing::error!("调度器异常: {e}");
                }
            } else {
                tracing::info!("定时调度未启用，常驻等待（托盘可手动触发）");
                let mut stop_rx_inner = stop_tx.subscribe();
                let mut trigger = scheduler.take_trigger();
                let sched_ref = &scheduler;
                loop {
                    tokio::select! {
                        _ = stop_rx_inner.changed() => break,
                        received = async {
                            match trigger.as_mut() {
                                Some(t) => t.recv().await,
                                None => std::future::pending().await,
                            }
                        } => {
                            if received.is_none() { break; }
                            let report = sched_ref.run_cycle(1).await;
                            if let Some(err) = &report.error {
                                tracing::error!("{err}");
                            }
                            sched_ref.report_to(&report);
                        }
                    }
                }
            }
        });
    });
}
