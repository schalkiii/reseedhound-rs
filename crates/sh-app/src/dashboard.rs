//! 仪表盘：指标卡 + 站点明细 + 日志流（审美升级版）。
//!
//! 视觉体系（theme.rs）：
//! - 深空背景 + 表面卡片分层（BG < SURFACE < SURFACE_HOVER）
//! - 品牌色渐变顶栏 + 状态徽章呼吸点
//! - 指标卡：大数字 + 小标签 + 趋势色，卡片等宽分布
//! - 日志面板：终端风深底等宽感

use std::sync::{Arc, Mutex};

use gpui::*;

use crate::theme;
use crate::tray::TrayCommand;

/// GUI 与后台调度共享的运行状态快照。
#[derive(Debug, Clone, Default)]
pub struct RunState {
    /// 定时调度是否启用（配置决定）。
    pub scheduler_on: bool,
    /// 本轮辅种是否正在执行。
    pub running: bool,
    pub cycle: u64,
    pub last_run: String,
    pub mode: String,
    pub total_torrents: u64,
    pub matched_count: u64,
    pub succeeded_count: u64,
    pub failed_count: u64,
    pub dead_count: u64,
    /// 站点明细（最近一轮）。
    pub sites: Vec<sh_engine::scheduler::SiteCycleSummary>,
    pub log_lines: Vec<String>,
}

impl RunState {
    pub fn push_log(&mut self, line: String) {
        self.log_lines.push(line);
        if self.log_lines.len() > 200 {
            self.log_lines.remove(0);
        }
    }
}

/// 主窗口页面路由。
#[derive(Clone, Copy, PartialEq)]
pub enum Page {
    Dashboard,
    Settings,
}

pub struct Dashboard {
    pub state: Arc<Mutex<RunState>>,
    pub trigger: tokio::sync::mpsc::Sender<()>,
    pub tray: std::sync::mpsc::Sender<TrayCommand>,
    pub page: Page,
    /// 设置页（懒初始化）
    pub settings: Option<Entity<crate::settings::SettingsPage>>,
    /// 配置读写中心（设置页用）
    pub config_store: Arc<crate::config_store::ConfigStore>,
}

impl Dashboard {
    /// 请求立即辅种（通道满 1 则忽略，天然防连点）。
    pub fn request_run_now(&self) {
        let _ = self.trigger.try_send(());
    }

    fn metric_card(label: &str, icon: &str, value: String, color: Rgba) -> impl IntoElement + use<> {
        div()
            .flex_1()
            .p_4()
            .rounded_xl()
            .bg(theme::SURFACE)
            .border_1()
            .border_color(theme::BORDER)
            .flex()
            .flex_col()
            .gap_2()
            .hover(|s| s.bg(theme::SURFACE_HOVER))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_sm().child(icon.to_string()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::TEXT_DIM)
                            .child(label.to_string()),
                    ),
            )
            .child(
                div()
                    .text_2xl()
                    .text_color(color)
                    .child(value),
            )
    }

    fn nav_button(
        label: &str,
        active: bool,
        id: &'static str,
    ) -> impl IntoElement + use<> {
        div()
            .id(id)
            .px_4()
            .py_1_5()
            .rounded_lg()
            .text_sm()
            .text_color(if active { rgb(0xffffff) } else { theme::TEXT_DIM })
            .bg(if active { theme::BTN_PRIMARY } else { theme::SURFACE })
            .border_1()
            .border_color(if active { theme::BTN_PRIMARY } else { theme::BORDER })
            .hover(|s| s.cursor_pointer())
            .child(label.to_string())
    }
}

impl Render for Dashboard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 页面路由：设置页独立渲染
        if self.page == Page::Settings {
            return self.render_settings_entry(cx).into_any_element();
        }

        let s = self.state.lock().unwrap().clone();
        let (status_label, status_color) = theme::status_badge(s.running, s.scheduler_on);
        let running = s.running;

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::BG)
            .p_6()
            .gap_4()
            // 顶栏：标题 + 状态徽章 + 导航 + 操作
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .text_xl()
                                    .text_color(theme::ACCENT)
                                    .child("ReseedHound RS"),
                            )
                            .child(
                                div()
                                    .px_3()
                                    .py_1()
                                    .rounded_full()
                                    .bg(theme::SURFACE)
                                    .border_1()
                                    .border_color(status_color)
                                    .text_xs()
                                    .text_color(status_color)
                                    .child(format!("● {status_label}")),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .items_center()
                            .child(Self::nav_button("仪表盘", true, "nav-dash"))
                            .child(
                                div()
                                    .id("nav-settings")
                                    .px_4()
                                    .py_1_5()
                                    .rounded_lg()
                                    .text_sm()
                                    .text_color(theme::TEXT_DIM)
                                    .bg(theme::SURFACE)
                                    .border_1()
                                    .border_color(theme::BORDER)
                                    .hover(|s| s.cursor_pointer().bg(theme::SURFACE_HOVER))
                                    .child("⚙ 设置")
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, _ev, _w, cx| {
                                            this.page = Page::Settings;
                                            cx.notify();
                                        }),
                                    ),
                            ),
                    ),
            )
            // 指标行 1
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(Self::metric_card("扫描种子", "📦", s.total_torrents.to_string(), theme::TEXT))
                    .child(Self::metric_card("全站匹配", "🎯", s.matched_count.to_string(), theme::BLUE))
                    .child(Self::metric_card("辅种成功", "🟢", s.succeeded_count.to_string(), theme::GREEN))
                    .child(Self::metric_card("下载失败", "🔻", s.failed_count.to_string(), theme::RED)),
            )
            // 指标行 2
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(Self::metric_card("死种跳过", "💀", s.dead_count.to_string(), theme::YELLOW))
                    .child(Self::metric_card("轮次", "⏱", s.cycle.to_string(), theme::TEXT))
                    .child(Self::metric_card("模式", "📡", s.mode.clone(), theme::TEXT))
                    .child(Self::metric_card("上次运行", "🕒", if s.last_run.is_empty() { "—".into() } else { s.last_run.clone() }, theme::TEXT)),
            )
            // 站点明细
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .children(if s.sites.is_empty() {
                        vec![div()
                            .text_sm()
                            .text_color(theme::TEXT_DIM)
                            .child("（站点明细将在首轮辅种后显示）")]
                    } else {
                        s.sites
                            .iter()
                            .map(|site| {
                                let (dot, color) = theme::status_dot(site.failed, site.succeeded, site.matched);
                                div()
                                    .px_3()
                                    .py_1_5()
                                    .rounded_md()
                                    .bg(theme::SURFACE)
                                    .border_1()
                                    .border_color(theme::BORDER)
                                    .text_sm()
                                    .child(format!(
                                        "{dot} {}  匹配 {} · 成功 {} · 失败 {}",
                                        site.name, site.matched, site.succeeded, site.failed
                                    ))
                                    .text_color(color)
                            })
                            .collect::<Vec<_>>()
                    }),
            )
            // 日志面板
            .child(
                div()
                    .id("log-panel")
                    .flex_1()
                    .min_h(px(120.))
                    .rounded_xl()
                    .bg(rgb(0x0d1117))
                    .border_1()
                    .border_color(theme::BORDER)
                    .p_4()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x9fb0c3))
                            .line_height(relative(1.5))
                            .child(if s.log_lines.is_empty() {
                                "（运行日志将在此显示）".to_string()
                            } else {
                                s.log_lines.join("\n")
                            }),
                    ),
            )
            // 底部操作
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_3()
                    .child(
                        div()
                            .id("run-now-btn")
                            .px_5()
                            .py_2()
                            .rounded_lg()
                            .bg(if s.running { theme::DISABLED } else { theme::BTN_PRIMARY })
                            .text_color(rgb(0xffffff))
                            .text_sm()
                            .hover(move |s| if running { s } else { s.cursor_pointer().bg(theme::BTN_PRIMARY_HOVER) })
                            .child(if s.running {
                                "⟳ 辅种运行中…".to_string()
                            } else {
                                "▶ 立即辅种".to_string()
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _ev, _window, _cx| {
                                    this.request_run_now();
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id("quit-btn")
                            .px_5()
                            .py_2()
                            .rounded_lg()
                            .bg(rgb(0x7f1d1d))
                            .text_color(rgb(0xfca5a5))
                            .text_sm()
                            .border_1()
                            .border_color(rgb(0x991b1b))
                            .hover(|s| s.cursor_pointer().bg(rgb(0x991b1b)))
                            .child("退出程序")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _ev, _window, _cx| {
                                    let _ = this.tray.send(TrayCommand::Quit);
                                }),
                            ),
                    ),
            )
            .into_any_element()
    }
}

impl Dashboard {
    /// 设置页入口：内嵌 SettingsPage entity（进入时创建，保存走行级写回）。
    fn render_settings_entry(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        // 懒初始化设置页（首次进入创建）
        if self.settings.is_none() {
            let store = self.config_store.clone();
            self.settings = Some(cx.new(|cx| crate::settings::SettingsPage::new(store, cx)));
        }
        let settings = self.settings.clone().unwrap();

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::BG)
            .p_6()
            .gap_4()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(div().text_xl().text_color(theme::ACCENT).child("设置"))
                    .child(
                        div()
                            .id("nav-back-dash")
                            .px_4()
                            .py_1_5()
                            .rounded_lg()
                            .text_sm()
                            .text_color(theme::TEXT_DIM)
                            .bg(theme::SURFACE)
                            .border_1()
                            .border_color(theme::BORDER)
                            .hover(|s| s.cursor_pointer().bg(theme::SURFACE_HOVER))
                            .child("← 返回仪表盘")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _ev, _w, cx| {
                                    this.page = Page::Dashboard;
                                    cx.notify();
                                }),
                            ),
                    ),
            )
            .child(settings)
    }
}
