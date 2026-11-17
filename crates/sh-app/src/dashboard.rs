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
    /// 演练模式（CLI --dry-run 对应）：只查询不添加种子。
    pub dry_run: bool,
    /// 最近一次工具动作（sync-cookies / check-dead / weekly-report）的结果文本。
    pub action_result: String,
    /// 工具动作是否正在执行。
    pub action_running: bool,
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

/// GUI 触发的后台工具动作（对齐 CLI 子命令）。
#[derive(Debug, Clone)]
pub enum GuiAction {
    /// sync-cookies：从 CookieCloud 同步站点 Cookie
    SyncCookies,
    /// check-dead：对照 savept.icu 死亡名单（不落盘，只出建议）
    CheckDead,
    /// weekly-report：生成最近 7 天周报
    WeeklyReport,
}

pub struct Dashboard {
    pub state: Arc<Mutex<RunState>>,
    pub trigger: tokio::sync::mpsc::Sender<()>,
    pub tray: std::sync::mpsc::Sender<TrayCommand>,
    /// 工具动作通道（sync-cookies / check-dead / weekly-report）
    pub actions: tokio::sync::mpsc::Sender<GuiAction>,
    /// 主配置路径（工具动作读取 config.yaml 用）
    pub config_path: String,
    /// sites.yaml 路径
    pub sites_path: String,
    pub page: Page,
    /// 设置页（进入设置页时创建，离开时丢弃——保证每次从磁盘加载最新值）
    pub settings: Option<Entity<crate::settings::SettingsPage>>,
    /// 配置读写中心（设置页用）
    pub config_store: Arc<crate::config_store::ConfigStore>,
}

impl Dashboard {
    /// 请求立即辅种（通道满 1 则忽略，天然防连点）。
    pub fn request_run_now(&self) {
        let _ = self.trigger.try_send(());
    }

    /// 请求执行工具动作（通道满 1 则忽略，防连点）。
    pub fn request_action(&self, action: GuiAction) {
        let _ = self.actions.try_send(action);
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
        let dry_run = s.dry_run;
        let action_running = s.action_running;
        let action_result = if s.action_result.is_empty() {
            None
        } else {
            Some(s.action_result.clone())
        };

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
            // 工具动作行（对齐 CLI 子命令）+ 结果面板
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .id("act-sync-cookies")
                                    .px_3().py_1_5()
                                    .rounded_lg()
                                    .text_xs()
                                    .text_color(theme::TEXT_DIM)
                                    .bg(theme::SURFACE)
                                    .border_1().border_color(theme::BORDER)
                                    .hover(|s| s.cursor_pointer().bg(theme::SURFACE_HOVER))
                                    .child("⟳ 同步 Cookie")
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, _ev, _w, _cx| {
                                            this.request_action(GuiAction::SyncCookies);
                                        }),
                                    ),
                            )
                            .child(
                                div()
                                    .id("act-check-dead")
                                    .px_3().py_1_5()
                                    .rounded_lg()
                                    .text_xs()
                                    .text_color(theme::TEXT_DIM)
                                    .bg(theme::SURFACE)
                                    .border_1().border_color(theme::BORDER)
                                    .hover(|s| s.cursor_pointer().bg(theme::SURFACE_HOVER))
                                    .child("☠ 死种检测")
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, _ev, _w, _cx| {
                                            this.request_action(GuiAction::CheckDead);
                                        }),
                                    ),
                            )
                            .child(
                                div()
                                    .id("act-weekly-report")
                                    .px_3().py_1_5()
                                    .rounded_lg()
                                    .text_xs()
                                    .text_color(theme::TEXT_DIM)
                                    .bg(theme::SURFACE)
                                    .border_1().border_color(theme::BORDER)
                                    .hover(|s| s.cursor_pointer().bg(theme::SURFACE_HOVER))
                                    .child("📋 生成周报")
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, _ev, _w, _cx| {
                                            this.request_action(GuiAction::WeeklyReport);
                                        }),
                                    ),
                            )
                            .child(
                                div()
                                    .id("dry-run-toggle")
                                    .px_3().py_1_5()
                                    .rounded_lg()
                                    .text_xs()
                                    .bg(if dry_run { theme::YELLOW } else { theme::SURFACE })
                                    .border_1()
                                    .border_color(if dry_run { theme::YELLOW } else { theme::BORDER })
                                    .text_color(if dry_run { rgb(0x1a1a1a) } else { theme::TEXT_DIM })
                                    .hover(|s| s.cursor_pointer())
                                    .child(if dry_run { "演练模式 ON".to_string() } else { "演练模式 OFF".to_string() })
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, _ev, _w, cx| {
                                            if let Ok(mut s) = this.state.lock() {
                                                s.dry_run = !s.dry_run;
                                                let now_dry = s.dry_run;
                                                s.push_log(if now_dry {
                                                    "[演练模式] 已开启：只查询站点，不添加种子".into()
                                                } else {
                                                    "[演练模式] 已关闭".into()
                                                });
                                            }
                                            cx.notify();
                                        }),
                                    ),
                            )
                            .child(if action_running {
                                div()
                                    .text_xs()
                                    .text_color(theme::TEXT_DIM)
                                    .child("执行中…")
                                    .into_any_element()
                            } else {
                                div().into_any_element()
                            }),
                    )
                    .children(action_result.map(|text| {
                        div()
                            .id("action-result")
                            .max_h(px(160.))
                            .overflow_y_scroll()
                            .p_3()
                            .rounded_md()
                            .bg(rgb(0x0d1117))
                            .border_1().border_color(theme::BORDER)
                            .text_xs()
                            .text_color(rgb(0x9fb0c3))
                            .line_height(relative(1.4))
                            .child(text)
                    })),
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
    /// 设置页入口：每次进入都从磁盘重建 SettingsPage（配置以磁盘为准，
    /// 避免向导/外部修改后显示陈旧值），保存走行级写回。
    fn render_settings_entry(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
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
                                    this.settings = None; // 离开时丢弃，下次进入重新从磁盘加载
                                    cx.notify();
                                }),
                            ),
                    ),
            )
            .child(
                // flex_1 + min_h_0：给 SettingsPage 一个可解析的弹性约束
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(settings),
            )
    }
}
