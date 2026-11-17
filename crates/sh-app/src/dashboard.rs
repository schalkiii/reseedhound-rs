//! 仪表盘：紧凑数据条 + 站点明细表 + 日志流（专业工具风）。
//!
//! 视觉原则（去 AI 味）：
//! - 不用 emoji 图标；数据用排版层级表达（数值大、标签小写灰）
//! - 指标不做成 8 张等宽卡片阵列，而是顶栏下的一行内联数据条
//! - 卡片少边框：靠背景明度分层（SURFACE vs BG）
//! - Tab 用下划线激活态而非彩色底块

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
    /// 演练模式（CLI --dry_run 对应）：只查询不添加种子。
    pub dry_run: bool,
    /// 最近一次工具动作（sync-cookies / check-dead / weekly-report）的结果文本。
    pub action_result: String,
    /// 工具动作是否正在执行。
    pub action_running: bool,
    /// 本轮站点范围（空 = 全部）。GUI 选择后由调度器 options_provider 注入 `--site`，
    /// 每轮结束由 on_report 清空——只影响下一轮，定时轮次仍跑全部站点。
    pub selected_sites: Vec<String>,
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
    pub page: Page,
    /// 设置页（进入设置页时创建，离开时丢弃——保证每次从磁盘加载最新值）
    pub settings: Option<Entity<crate::settings::SettingsPage>>,
    /// 配置读写中心（设置页用）
    pub config_store: Arc<crate::config_store::ConfigStore>,
    /// 站点选择面板是否展开（本轮站点范围）
    pub picker_open: bool,
    /// 可选站点名（打开面板时从 sites.yaml 加载，仅 enabled 站点）
    pub site_names: Vec<String>,
}

impl Dashboard {
    /// 打开/关闭站点范围面板；打开时从磁盘重读启用站点名。
    fn toggle_picker(&mut self, cx: &mut Context<Self>) {
        self.picker_open = !self.picker_open;
        if self.picker_open {
            self.site_names = self
                .config_store
                .load_sites_form()
                .map(|forms| {
                    forms
                        .into_iter()
                        .filter(|form| form.enabled && !form.name.is_empty())
                        .map(|form| form.name)
                        .collect()
                })
                .unwrap_or_default();
        }
        cx.notify();
    }

    /// 切换单个站点的选中态（写入共享 RunState，调度器下轮读取）。
    fn toggle_site(&mut self, name: &str, cx: &mut Context<Self>) {
        if let Ok(mut s) = self.state.lock() {
            match s.selected_sites.iter().position(|n| n == name) {
                Some(idx) => {
                    s.selected_sites.remove(idx);
                }
                None => s.selected_sites.push(name.to_string()),
            }
        }
        cx.notify();
    }

    /// 一次性设置全部站点（全选 / 清空）。
    fn set_all_sites(&mut self, all: bool, cx: &mut Context<Self>) {
        if let Ok(mut s) = self.state.lock() {
            s.selected_sites = if all {
                self.site_names.clone()
            } else {
                Vec::new()
            };
        }
        cx.notify();
    }
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

    /// 数据条单元：小灰标签 + 数值（无框无卡，纯排版）。
    fn stat(label: &str, value: String, color: Rgba) -> impl IntoElement + use<> {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(theme::TEXT_FAINT)
                    .child(label.to_string()),
            )
            .child(div().text_lg().text_color(color).font_family("Consolas").child(value))
    }

    /// 顶栏右侧文字按钮（ghost 风格，无底色边框）。
    fn header_button(
        label: &str,
        id: &'static str,
        handler: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> AnyElement {
        div()
            .id(id)
            .px_2()
            .py_1()
            .rounded_md()
            .text_sm()
            .text_color(theme::TEXT_DIM)
            .hover(|s| s.cursor_pointer().bg(theme::BTN_GHOST_HOVER))
            .child(label.to_string())
            .on_mouse_down(MouseButton::Left, handler)
            .into_any_element()
    }

    /// 工具动作按钮（文字按钮，语义弱于主操作）。
    fn tool_button(
        label: &str,
        id: &'static str,
        handler: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> AnyElement {
        div()
            .id(id)
            .px_2()
            .py_1()
            .rounded_md()
            .text_xs()
            .text_color(theme::TEXT_DIM)
            .hover(|s| s.cursor_pointer().bg(theme::BTN_GHOST_HOVER))
            .child(label.to_string())
            .on_mouse_down(MouseButton::Left, handler)
            .into_any_element()
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
        let selected_sites = s.selected_sites.clone();
        let picker_open = self.picker_open;
        let site_names = self.site_names.clone();

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::BG)
            .p_5()
            .gap_0()
            // 顶栏：产品名（普通白色字重）+ 状态 + 导航
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .pb_3()
                    .border_b_1()
                    .border_color(theme::DIVIDER)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .text_base()
                                    .text_color(theme::TEXT)
                                    .child("ReseedHound"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1p5()
                                    .child(
                                        div()
                                            .w(px(6.))
                                            .h(px(6.))
                                            .rounded_full()
                                            .bg(status_color),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme::TEXT_DIM)
                                            .child(status_label.to_string()),
                                    ),
                            )
                            .children(if dry_run {
                                vec![div()
                                    .px_1p5()
                                    .py(px(1.))
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(theme::YELLOW)
                                    .text_xs()
                                    .text_color(theme::YELLOW)
                                    .child("演练")]
                            } else {
                                vec![]
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .items_center()
                            .child(Self::header_button("设置", "nav-settings", cx.listener(
                                |this, _ev, _w, cx| {
                                    this.page = Page::Settings;
                                    cx.notify();
                                },
                            ))),
                    ),
            )
            // 数据条：一行内联指标（无卡片）
            .child(
                div()
                    .flex()
                    .gap_8()
                    .py_3()
                    .child(Self::stat("种子", s.total_torrents.to_string(), theme::TEXT))
                    .child(Self::stat("匹配", s.matched_count.to_string(), theme::TEXT))
                    .child(Self::stat("成功", s.succeeded_count.to_string(), theme::GREEN))
                    .child(Self::stat("失败", s.failed_count.to_string(), theme::RED))
                    .child(Self::stat("死种", s.dead_count.to_string(), theme::YELLOW))
                    .child(Self::stat("轮次", s.cycle.to_string(), theme::TEXT_DIM))
                    .child(Self::stat("模式", if s.mode.is_empty() { "—".into() } else { s.mode.clone() }, theme::TEXT_DIM))
                    .child(if s.last_run.is_empty() {
                        Self::stat("上次运行", "—".into(), theme::TEXT_DIM)
                    } else {
                        Self::stat("上次运行", s.last_run.clone(), theme::TEXT_DIM)
                    }),
            )
            // 主区：左站点明细 / 右日志
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .gap_3()
                    // 站点明细（表式，等宽数字）
                    .child(
                        div()
                            .id("site-list")
                            .w(px(280.))
                            .flex()
                            .flex_col()
                            .min_h_0()
                            .overflow_y_scroll()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::TEXT_FAINT)
                                    .pb_2()
                                    .child("站点"),
                            )
                            .children(if s.sites.is_empty() {
                                vec![div()
                                    .text_sm()
                                    .text_color(theme::TEXT_FAINT)
                                    .child("首轮辅种后显示")]
                            } else {
                                s.sites
                                    .iter()
                                    .map(|site| {
                                        let (dot, color) = theme::status_dot(site.failed, site.succeeded, site.matched);
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .py_1p5()
                                            .border_b_1()
                                            .border_color(theme::DIVIDER)
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(color)
                                                    .child(dot.to_string()),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .text_sm()
                                                    .text_color(theme::TEXT)
                                                    .child(site.name.clone()),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(theme::TEXT_DIM)
                                                    .font_family("Consolas")
                                                    .child(format!(
                                                        "{}/{}",
                                                        site.succeeded, site.matched
                                                    )),
                                            )
                                            .children(if site.failed > 0 {
                                                vec![div()
                                                    .text_xs()
                                                    .text_color(theme::RED)
                                                    .font_family("Consolas")
                                                    .child(format!("−{}", site.failed))]
                                            } else {
                                                vec![]
                                            })
                                    })
                                    .collect::<Vec<_>>()
                            }),
                    )
                    // 日志面板（终端风）
                    .child(
                        div()
                            .id("log-panel")
                            .flex_1()
                            .min_h_0()
                            .rounded_md()
                            .bg(theme::INPUT_BG)
                            .p_3()
                            .overflow_y_scroll()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme::TEXT_DIM)
                                    .font_family("Consolas")
                                    .line_height(relative(1.6))
                                    .child(if s.log_lines.is_empty() {
                                        "等待运行…".to_string()
                                    } else {
                                        s.log_lines.join("\n")
                                    }),
                            ),
                    ),
            )
            // 站点范围面板（本轮只跑选中站点；每轮结束自动恢复全部）
            .child(if picker_open {
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .pb_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::TEXT_FAINT)
                                    .child(format!(
                                        "站点范围 · 已选 {} / {}（本轮生效，跑完自动恢复全部）",
                                        selected_sites.len(),
                                        site_names.len()
                                    )),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .child(Self::tool_button("全选", "site-pick-all", cx.listener(
                                        |this, _ev, _w, cx| this.set_all_sites(true, cx),
                                    )))
                                    .child(Self::tool_button("清空", "site-pick-none", cx.listener(
                                        |this, _ev, _w, cx| this.set_all_sites(false, cx),
                                    )))
                                    .child(Self::tool_button("收起", "site-pick-close", cx.listener(
                                        |this, _ev, _w, cx| {
                                            this.picker_open = false;
                                            cx.notify();
                                        },
                                    ))),
                            ),
                    )
                    .child(
                        div()
                            .id("site-picker")
                            .max_h(px(176.))
                            .overflow_y_scroll()
                            .flex()
                            .flex_wrap()
                            .gap_1()
                            .children(site_names.iter().enumerate().map(|(idx, name)| {
                                let name = name.clone();
                                let selected = selected_sites.iter().any(|n| n == &name);
                                let toggle_name = name.clone();
                                div()
                                    .id(("site-pick", idx))
                                    .px_2()
                                    .py(px(3.))
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(if selected { theme::ACCENT } else { theme::DIVIDER })
                                    .bg(if selected { theme::SURFACE } else { theme::INPUT_BG })
                                    .text_xs()
                                    .text_color(if selected { theme::TEXT } else { theme::TEXT_DIM })
                                    .hover(|s| s.cursor_pointer())
                                    .child(format!("{} {name}", if selected { "●" } else { "○" }))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _ev, _w, cx| {
                                            this.toggle_site(&toggle_name, cx);
                                        }),
                                    )
                            })),
                    )
                    .into_any_element()
            } else {
                div().into_any_element()
            })
            // 工具动作行 + 结果
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .pt_3()
                    .mt_1()
                    .border_t_1()
                    .border_color(theme::DIVIDER)
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .items_center()
                            .child(Self::tool_button(
                                &if selected_sites.is_empty() {
                                    "站点范围 · 全部".to_string()
                                } else {
                                    format!("站点范围 · 已选 {} 个", selected_sites.len())
                                },
                                "act-site-scope",
                                cx.listener(|this, _ev, _w, cx| this.toggle_picker(cx)),
                            ))
                            .child(Self::tool_button("同步 Cookie", "act-sync-cookies", cx.listener(
                                |this, _ev, _w, _cx| {
                                    this.request_action(GuiAction::SyncCookies);
                                },
                            )))
                            .child(Self::tool_button("死种检测", "act-check-dead", cx.listener(
                                |this, _ev, _w, _cx| {
                                    this.request_action(GuiAction::CheckDead);
                                },
                            )))
                            .child(Self::tool_button("生成周报", "act-weekly-report", cx.listener(
                                |this, _ev, _w, _cx| {
                                    this.request_action(GuiAction::WeeklyReport);
                                },
                            )))
                            .child(
                                div()
                                    .id("dry-run-toggle")
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .text_xs()
                                    .text_color(if dry_run { theme::YELLOW } else { theme::TEXT_DIM })
                                    .hover(|s| s.cursor_pointer().bg(theme::BTN_GHOST_HOVER))
                                    .child(if dry_run { "演练模式 · 开" } else { "演练模式 · 关" })
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
                                    .text_color(theme::TEXT_FAINT)
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
                            .bg(theme::INPUT_BG)
                            .text_xs()
                            .text_color(theme::TEXT_DIM)
                            .font_family("Consolas")
                            .line_height(relative(1.4))
                            .child(text)
                    })),
            )
            // 底部操作
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .pt_3()
                    .child(
                        div()
                            .id("quit-btn")
                            .px_3()
                            .py_1p5()
                            .rounded_md()
                            .text_sm()
                            .text_color(theme::TEXT_FAINT)
                            .hover(|s| s.cursor_pointer().text_color(theme::RED).bg(theme::BTN_GHOST_HOVER))
                            .child("退出")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _ev, _window, _cx| {
                                    let _ = this.tray.send(TrayCommand::Quit);
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id("run-now-btn")
                            .px_4()
                            .py_1p5()
                            .rounded_md()
                            .bg(if s.running { theme::DISABLED } else { theme::BTN_PRIMARY })
                            .text_color(theme::TEXT_ON_PRIMARY)
                            .text_sm()
                            .hover(move |s| if running { s } else { s.cursor_pointer().bg(theme::BTN_PRIMARY_HOVER) })
                            .child(if s.running {
                                "辅种运行中…".to_string()
                            } else {
                                "立即辅种".to_string()
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _ev, _window, _cx| {
                                    this.request_run_now();
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
            .p_5()
            .gap_0()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .pb_3()
                    .border_b_1()
                    .border_color(theme::DIVIDER)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(Self::header_button("←", "nav-back-dash", cx.listener(
                                |this, _ev, _w, cx| {
                                    this.page = Page::Dashboard;
                                    this.settings = None; // 离开时丢弃，下次进入重新从磁盘加载
                                    cx.notify();
                                },
                            )))
                            .child(
                                div()
                                    .text_base()
                                    .text_color(theme::TEXT)
                                    .child("设置"),
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
                    .pt_4()
                    .child(settings),
            )
    }
}
