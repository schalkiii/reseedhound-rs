//! 设置页：直接编辑 config.yaml / sites.yaml 的常用字段。
//!
//! 三个分区（标签页）：
//! - 下载器：类型/地址/端口/账号/密码/种子目录
//! - 站点：列表（启用开关 + 编辑）+ 新增
//! - 调度与全局：定时开关/间隔/启动即跑 + 并发/批量/模式
//!
//! 写回策略：行级就地更新保留注释（config_store），保存后下一轮辅种自动生效
//! （调度器每轮从磁盘重建引擎）。passkey/password 输入框用遮罩显示。

use std::sync::Arc;

use gpui::*;
use unicode_segmentation::UnicodeSegmentation;

use crate::config_store::{ConfigStore, DownloaderForm, SiteForm};
use crate::text_input::TextInput;
use crate::theme;

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Downloader,
    Sites,
    Scheduler,
}

pub struct SettingsPage {
    tab: Tab,
    store: Arc<ConfigStore>,
    // 下载器
    dl_type: Entity<TextInput>,
    dl_host: Entity<TextInput>,
    dl_port: Entity<TextInput>,
    dl_user: Entity<TextInput>,
    dl_pass: Entity<TextInput>,
    dl_dir: Entity<TextInput>,
    // 站点列表（当前编辑态）
    sites: Vec<SiteRow>,
    // 调度 & 全局
    sched_enabled: bool,
    sched_interval: Entity<TextInput>,
    sched_run_on_start: bool,
    glob_concurrency: Entity<TextInput>,
    glob_batch: Entity<TextInput>,
    glob_mode: Entity<TextInput>,
    // 状态
    status: Option<(String, bool)>, // (消息, 成功?)
}

/// 站点行：表单模型 + 输入框实体
struct SiteRow {
    form: SiteForm,
    name: Entity<TextInput>,
    url: Entity<TextInput>,
    passkey: Entity<TextInput>,
}

impl SettingsPage {
    pub fn new(store: Arc<ConfigStore>, cx: &mut Context<Self>) -> Self {
        let input = |placeholder: &str, initial: &str, cx: &mut Context<Self>| {
            cx.new(|cx| TextInput::new(cx, placeholder, initial, false))
        };
        let masked = |placeholder: &str, initial: &str, cx: &mut Context<Self>| {
            cx.new(|cx| TextInput::new(cx, placeholder, initial, true))
        };

        // 从磁盘加载当前值
        let dl = store.load_downloader_form().unwrap_or_default();
        let sites = store.load_sites_form().unwrap_or_default();
        let config_text = store.read_config_text().unwrap_or_default();
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(&config_text).unwrap_or(serde_yaml::Value::Null);
        let sched = parsed.get("scheduler").cloned().unwrap_or(serde_yaml::Value::Null);
        let global = parsed.get("global").cloned().unwrap_or(serde_yaml::Value::Null);
        let gget = |k: &str, d: &str| -> String {
            global.get(k).and_then(|v| v.as_str()).map(|s| s.to_string())
                .or_else(|| global.get(k).and_then(|v| v.as_i64()).map(|n| n.to_string()))
                .unwrap_or_else(|| d.to_string())
        };

        let site_rows = sites
            .into_iter()
            .map(|form| {
                let name = cx.new(|cx| TextInput::new(cx, "站点名", &form.name, false));
                let url = cx.new(|cx| TextInput::new(cx, "https://…", &form.url, false));
                let passkey = cx.new(|cx| TextInput::new(cx, "passkey", &form.passkey, true));
                SiteRow { form, name, url, passkey }
            })
            .collect();

        Self {
            tab: Tab::Downloader,
            store,
            dl_type: input("qbittorrent", &dl.dl_type, cx),
            dl_host: input("127.0.0.1", &dl.host, cx),
            dl_port: input("8080", &dl.port, cx),
            dl_user: input("用户名", &dl.username, cx),
            dl_pass: masked("密码", &dl.password, cx),
            dl_dir: input("种子目录", &dl.torrent_dir, cx),
            sites: site_rows,
            sched_enabled: sched.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false),
            sched_interval: input("360", &sched.get("interval_minutes")
                .and_then(|v| v.as_f64()).map(|n| format!("{n}")).unwrap_or_default(), cx),
            sched_run_on_start: sched.get("run_on_start").and_then(|v| v.as_bool()).unwrap_or(true),
            glob_concurrency: input("20", &gget("concurrency", "20"), cx),
            glob_batch: input("100", &gget("batch_size", "100"), cx),
            glob_mode: input("pieces_hash", &gget("mode", "pieces_hash"), cx),
            status: None,
        }
    }

    fn field_row(label: &str, input: &Entity<TextInput>, hint: Option<&str>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_sm().text_color(theme::TEXT_DIM).child(label.to_string()))
            .child(input.clone())
            .children(hint.map(|h| div().text_xs().text_color(theme::TEXT_DIM).child(h.to_string())))
            .into_any_element()
    }

    fn tab_button(label: &str, active: bool, id: &'static str, tab: Tab) -> AnyElement {
        let _ = tab; // 切换由 render 的 on_mouse_down 统一处理
        div()
            .id(id)
            .px_4().py_2()
            .rounded_lg()
            .text_sm()
            .text_color(if active { rgb(0xffffff) } else { theme::TEXT_DIM })
            .bg(if active { theme::BTN_PRIMARY } else { theme::SURFACE })
            .hover(|s| s.cursor_pointer())
            .child(label.to_string())
            .into_any_element()
    }

    fn collect_site_forms(&self, cx: &App) -> Vec<SiteForm> {
        self.sites
            .iter()
            .map(|row| SiteForm {
                name: row.name.read(cx).value(),
                url: row.url.read(cx).value(),
                passkey: row.passkey.read(cx).value(),
                api_url: row.form.api_url.clone(),
                enabled: row.form.enabled,
            })
            .collect()
    }

    fn save_all(&mut self, cx: &mut Context<Self>) {
        let result = (|| -> anyhow::Result<()> {
            // 下载器
            let dl = DownloaderForm {
                dl_type: self.dl_type.read(cx).value(),
                host: self.dl_host.read(cx).value(),
                port: self.dl_port.read(cx).value(),
                username: self.dl_user.read(cx).value(),
                password: self.dl_pass.read(cx).value(),
                torrent_dir: self.dl_dir.read(cx).value(),
            };
            self.store.save_downloader(&dl)?;
            // 站点
            let forms = self.collect_site_forms(cx);
            if !forms.is_empty() {
                self.store.save_sites(&forms)?;
            }
            // 调度 & 全局
            let interval: f64 = self.sched_interval.read(cx).value().parse().unwrap_or(360.0);
            self.store.save_scheduler(self.sched_enabled, interval, self.sched_run_on_start)?;
            let concurrency: usize = self.glob_concurrency.read(cx).value().parse().unwrap_or(20);
            let batch: usize = self.glob_batch.read(cx).value().parse().unwrap_or(100);
            self.store.save_global(concurrency, batch, &self.glob_mode.read(cx).value())?;
            Ok(())
        })();
        match result {
            Ok(()) => self.status = Some(("已保存，下一轮辅种自动生效".into(), true)),
            Err(e) => self.status = Some((format!("保存失败: {e}"), false)),
        }
        cx.notify();
    }
}

impl Render for SettingsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = self.tab;
        let status = self.status.clone();

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::BG)
            .p_8()
            .gap_5()
            // 标签栏
            .child(
                div()
                    .flex()
                    .gap_2()
                    .children(vec![
                        Self::tab_button("下载器", tab == Tab::Downloader, "tab-dl", Tab::Downloader),
                        Self::tab_button("站点", tab == Tab::Sites, "tab-sites", Tab::Sites),
                        Self::tab_button("调度与全局", tab == Tab::Scheduler, "tab-sched", Tab::Scheduler),
                    ]),
            )
            // 内容区（滚动）
            .child(
                div()
                    .id("settings-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(
                        div()
                            .p_6()
                            .rounded_xl()
                            .bg(theme::SURFACE)
                            .border_1()
                            .border_color(theme::BORDER)
                            .flex()
                            .flex_col()
                            .gap_4()
                            .children(match tab {
                                Tab::Downloader => vec![
                                    Self::field_row("下载器类型（qbittorrent / transmission）", &self.dl_type, None),
                                    div().flex().gap_3()
                                        .child(div().flex_1().child(Self::field_row("地址", &self.dl_host, None)))
                                        .child(div().w_32().child(Self::field_row("端口", &self.dl_port, None)))
                                        .into_any_element(),
                                    Self::field_row("用户名", &self.dl_user, None),
                                    Self::field_row("密码", &self.dl_pass, None),
                                    Self::field_row("种子目录", &self.dl_dir,
                                        Some("保存后下一轮辅种生效（无需重启）")),
                                ],
                                Tab::Sites => {
                                    let mut rows: Vec<AnyElement> = self.sites.iter().enumerate().map(|(idx, row)| {
                                        let enabled = row.form.enabled;
                                        let (dot, color) = if enabled {
                                            ("●", theme::GREEN)
                                        } else {
                                            ("○", theme::TEXT_DIM)
                                        };
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap_2()
                                            .p_4()
                                            .rounded_lg()
                                            .bg(theme::INPUT_BG)
                                            .border_1()
                                            .border_color(theme::BORDER)
                                            .child(
                                                div().flex().justify_between().items_center()
                                                    .child(
                                                        div().text_sm().text_color(color)
                                                            .child(format!("{dot} 站点 #{}", idx + 1)),
                                                    )
                                                    .child(
                                                        div()
                                                            .id(("site-toggle", idx))
                                                            .px_3().py_1()
                                                            .rounded_md()
                                                            .text_xs()
                                                            .bg(if enabled { theme::BTN_PRIMARY } else { theme::SURFACE })
                                                            .text_color(if enabled { rgb(0xffffff) } else { theme::TEXT_DIM })
                                                            .hover(|s| s.cursor_pointer())
                                                            .child(if enabled { "已启用" } else { "已禁用" })
                                                            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _ev, _w, cx| {
                                                                if let Some(r) = this.sites.get_mut(idx) {
                                                                    r.form.enabled = !r.form.enabled;
                                                                }
                                                                cx.notify();
                                                            })),
                                                    ),
                                            )
                                            .child(div().flex().gap_3()
                                                .child(div().flex_1().child(Self::field_row("名称", &row.name, None)))
                                                .child(div().flex_1().child(Self::field_row("主页", &row.url, None))))
                                            .child(Self::field_row("Passkey", &row.passkey, None))
                                            .into_any_element()
                                    }).collect();
                                    // 新增站点按钮
                                    rows.push(
                                        div()
                                            .id("site-add")
                                            .px_4().py_2()
                                            .rounded_lg()
                                            .text_sm()
                                            .text_color(theme::LINK)
                                            .border_1().border_color(theme::BORDER)
                                            .hover(|s| s.cursor_pointer().bg(theme::BTN_GHOST_HOVER))
                                            .child("+ 新增站点")
                                            .on_mouse_down(MouseButton::Left, cx.listener(|this, _ev, _w, cx| {
                                                let name = cx.new(|cx| TextInput::new(cx, "站点名", "", false));
                                                let url = cx.new(|cx| TextInput::new(cx, "https://…", "", false));
                                                let passkey = cx.new(|cx| TextInput::new(cx, "passkey", "", true));
                                                this.sites.push(SiteRow {
                                                    form: SiteForm { name: String::new(), url: String::new(), passkey: String::new(), api_url: String::new(), enabled: true },
                                                    name, url, passkey,
                                                });
                                                cx.notify();
                                            }))
                                            .into_any_element(),
                                    );
                                    rows
                                }
                                Tab::Scheduler => vec![
                                    // 定时开关
                                    div().flex().justify_between().items_center()
                                        .child(div().flex().flex_col().gap_1()
                                            .child(div().text_sm().text_color(theme::TEXT).child("启用定时调度"))
                                            .child(div().text_xs().text_color(theme::TEXT_DIM).child("关闭时仅托盘/按钮手动触发辅种")))
                                        .child(
                                            div()
                                                .id("sched-toggle")
                                                .px_3().py_1()
                                                .rounded_md()
                                                .text_xs()
                                                .bg(if self.sched_enabled { theme::BTN_PRIMARY } else { theme::SURFACE })
                                                .text_color(if self.sched_enabled { rgb(0xffffff) } else { theme::TEXT_DIM })
                                                .hover(|s| s.cursor_pointer())
                                                .child(if self.sched_enabled { "已启用" } else { "已禁用" })
                                                .on_mouse_down(MouseButton::Left, cx.listener(|this, _ev, _w, cx| {
                                                    this.sched_enabled = !this.sched_enabled;
                                                    cx.notify();
                                                })),
                                        )
                                        .into_any_element(),
                                    Self::field_row("调度间隔（分钟）", &self.sched_interval, None),
                                    // run_on_start 开关
                                    div().flex().justify_between().items_center()
                                        .child(div().flex().flex_col().gap_1()
                                            .child(div().text_sm().text_color(theme::TEXT).child("启动即执行首轮"))
                                            .child(div().text_xs().text_color(theme::TEXT_DIM).child("关闭则等待一个完整间隔后再跑")))
                                        .child(
                                            div()
                                                .id("ros-toggle")
                                                .px_3().py_1()
                                                .rounded_md()
                                                .text_xs()
                                                .bg(if self.sched_run_on_start { theme::BTN_PRIMARY } else { theme::SURFACE })
                                                .text_color(if self.sched_run_on_start { rgb(0xffffff) } else { theme::TEXT_DIM })
                                                .hover(|s| s.cursor_pointer())
                                                .child(if self.sched_run_on_start { "已启用" } else { "已禁用" })
                                                .on_mouse_down(MouseButton::Left, cx.listener(|this, _ev, _w, cx| {
                                                    this.sched_run_on_start = !this.sched_run_on_start;
                                                    cx.notify();
                                                })),
                                        )
                                        .into_any_element(),
                                    div().h_px().bg(theme::BORDER).into_any_element(),
                                    Self::field_row("站点查询并发数", &self.glob_concurrency, None),
                                    Self::field_row("API 批量大小", &self.glob_batch, None),
                                    Self::field_row("辅种模式（pieces_hash / jackett / both）", &self.glob_mode, None),
                                ],
                            }),
                    ),
            )
            // 底部：状态 + 保存按钮
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .children(status.map(|(msg, ok)| {
                        div()
                            .text_sm()
                            .text_color(if ok { theme::GREEN } else { theme::RED })
                            .child(if ok { format!("✓ {msg}") } else { format!("⚠ {msg}") })
                    }))
                    .child(
                        div()
                            .id("settings-save")
                            .px_6().py_2()
                            .rounded_lg()
                            .bg(theme::BTN_PRIMARY)
                            .text_color(rgb(0xffffff))
                            .text_sm()
                            .hover(|s| s.cursor_pointer().bg(theme::BTN_PRIMARY_HOVER))
                            .child("保存全部")
                            .on_mouse_down(MouseButton::Left, cx.listener(|this, _ev, _w, cx| {
                                this.save_all(cx);
                            })),
                    ),
            )
    }
}

#[allow(dead_code)]
fn truncate_chars(s: &str, n: usize) -> String {
    s.grapheme_indices(true).take(n).map(|(_, g)| g).collect()
}
