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

use crate::config_store::{
    ConfigStore, CookieCloudForm, DownloaderForm, FeishuForm, JackettForm, SiteForm,
};
use crate::text_input::TextInput;
use crate::theme;

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Downloader,
    Sites,
    Scheduler,
    Notify,
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
    dl_tag: Entity<TextInput>,
    dl_skip_hash_check: bool,
    dl_auto_start: bool,
    // 站点列表（当前编辑态）
    sites: Vec<SiteRow>,
    // 调度 & 全局
    sched_enabled: bool,
    sched_interval: Entity<TextInput>,
    sched_run_on_start: bool,
    glob_concurrency: Entity<TextInput>,
    glob_batch: Entity<TextInput>,
    glob_mode: Entity<TextInput>,
    // 通知（飞书）+ CookieCloud + Jackett + 日志
    feishu_webhook: Entity<TextInput>,
    feishu_secret: Entity<TextInput>,
    cc_enabled: bool,
    cc_url: Entity<TextInput>,
    cc_uuid: Entity<TextInput>,
    cc_pass: Entity<TextInput>,
    jackett_enabled: bool,
    jackett_url: Entity<TextInput>,
    jackett_apikey: Entity<TextInput>,
    jackett_concurrency: Entity<TextInput>,
    jackett_timeout: Entity<TextInput>,
    log_level: Entity<TextInput>,
    log_file: Entity<TextInput>,
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
        let feishu = store.load_feishu_form().unwrap_or_default();
        let cc = store.load_cookiecloud_form().unwrap_or_default();
        let jackett = store.load_jackett_form().unwrap_or_default();
        let (log_level, log_file) = store.load_log_form().unwrap_or_else(|_| ("INFO".into(), "logs/reseed.log".into()));
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
            dl_tag: input("SeedHound", &dl.tag, cx),
            dl_skip_hash_check: dl.skip_hash_check.unwrap_or(true),
            dl_auto_start: dl.auto_start.unwrap_or(true),
            sites: site_rows,
            sched_enabled: sched.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false),
            sched_interval: input("360", &sched.get("interval_minutes")
                .and_then(|v| v.as_f64()).map(|n| format!("{n}")).unwrap_or_default(), cx),
            sched_run_on_start: sched.get("run_on_start").and_then(|v| v.as_bool()).unwrap_or(true),
            glob_concurrency: input("20", &gget("concurrency", "20"), cx),
            glob_batch: input("100", &gget("batch_size", "100"), cx),
            glob_mode: input("pieces_hash", &gget("mode", "pieces_hash"), cx),
            feishu_webhook: input("https://open.feishu.cn/…", &feishu.webhook_url, cx),
            feishu_secret: masked("签名校验 secret（可空）", &feishu.secret, cx),
            cc_enabled: cc.enabled,
            cc_url: input("http://127.0.0.1:8082/cookie", &cc.url, cx),
            cc_uuid: input("UUID", &cc.uuid, cx),
            cc_pass: masked("解密密码", &cc.password, cx),
            jackett_enabled: jackett.enabled,
            jackett_url: input("http://localhost:9117", &jackett.url, cx),
            jackett_apikey: masked("api_key", &jackett.api_key, cx),
            jackett_concurrency: input("5", &jackett.concurrency, cx),
            jackett_timeout: input("30", &jackett.timeout, cx),
            log_level: input("INFO", &log_level, cx),
            log_file: input("logs/reseed.log", &log_file, cx),
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

    fn tab_button(
        label: &str,
        active: bool,
        id: &'static str,
        tab: Tab,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(id)
            .px_3()
            .pb_2()
            .text_sm()
            .text_color(if active { theme::TEXT } else { theme::TEXT_DIM })
            .border_b_2()
            .border_color(if active { theme::ACCENT } else { theme::TRANSPARENT })
            .hover(|s| if active { s } else { s.cursor_pointer().text_color(theme::TEXT) })
            .child(label.to_string())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _ev, _w, cx| {
                    this.tab = tab;
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    /// 开关轨道 + 滑块（哑光，无文字）。
    fn switch(on: bool) -> Div {
        let (track, knob) = if on {
            (theme::ACCENT, theme::TEXT_ON_PRIMARY)
        } else {
            (theme::DISABLED, theme::TEXT_DIM)
        };
        let track_div = div()
            .flex()
            .flex_none()
            .w(px(28.))
            .h(px(16.))
            .rounded_full()
            .bg(track)
            .items_center()
            .px(px(2.));
        if on {
            track_div.child(div().ml_auto().w(px(12.)).h(px(12.)).rounded_full().bg(knob))
        } else {
            track_div.child(div().w(px(12.)).h(px(12.)).rounded_full().bg(knob))
        }
    }

    fn toggle_row(
        label: &str,
        hint: &str,
        on: bool,
        id: &'static str,
        handler: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> AnyElement {
        div()
            .flex()
            .justify_between()
            .items_center()
            .gap_4()
            .child(
                div().flex().flex_col().gap_1()
                    .child(div().text_sm().text_color(theme::TEXT).child(label.to_string()))
                    .child(div().text_xs().text_color(theme::TEXT_FAINT).child(hint.to_string())),
            )
            .child(
                div()
                    .id(id)
                    .hover(|s| s.cursor_pointer())
                    .child(Self::switch(on))
                    .on_mouse_down(MouseButton::Left, handler),
            )
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
        // 保存前校验（对齐 CLI 参数校验行为，坏值拒绝落盘）
        let port = self.dl_port.read(cx).value();
        if !port.is_empty() && port.parse::<u16>().is_err() {
            self.status = Some((format!("端口必须是 0-65535 的数字（当前: {port}）"), false));
            cx.notify();
            return;
        }
        let interval = self.sched_interval.read(cx).value();
        if !interval.is_empty() && interval.parse::<f64>().map_or(true, |v| v <= 0.0) {
            self.status = Some((format!("调度间隔必须是正数分钟（当前: {interval}）"), false));
            cx.notify();
            return;
        }
        let mode = self.glob_mode.read(cx).value();
        if !matches!(mode.as_str(), "pieces_hash" | "jackett" | "both") {
            self.status = Some((format!("辅种模式仅支持 pieces_hash / jackett / both（当前: {mode}）"), false));
            cx.notify();
            return;
        }
        // 磁盘上的当前开关状态（用于判断调度开关是否被本次保存翻转）
        let sched_was_enabled = self
            .store
            .read_config_text()
            .ok()
            .and_then(|t| serde_yaml::from_str::<serde_yaml::Value>(&t).ok())
            .and_then(|v| {
                v.get("scheduler")
                    .and_then(|s| s.get("enabled"))
                    .and_then(|b| b.as_bool())
            })
            .unwrap_or(false);
        let result = (|| -> anyhow::Result<()> {
            // 下载器
            let dl = DownloaderForm {
                dl_type: self.dl_type.read(cx).value(),
                host: self.dl_host.read(cx).value(),
                port: self.dl_port.read(cx).value(),
                username: self.dl_user.read(cx).value(),
                password: self.dl_pass.read(cx).value(),
                torrent_dir: self.dl_dir.read(cx).value(),
                tag: self.dl_tag.read(cx).value(),
                skip_hash_check: Some(self.dl_skip_hash_check),
                auto_start: Some(self.dl_auto_start),
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
            // 通知 / CookieCloud / Jackett / 日志
            let feishu = FeishuForm {
                webhook_url: self.feishu_webhook.read(cx).value(),
                secret: self.feishu_secret.read(cx).value(),
            };
            self.store.save_feishu(&feishu)?;
            let cc = CookieCloudForm {
                enabled: self.cc_enabled,
                url: self.cc_url.read(cx).value(),
                uuid: self.cc_uuid.read(cx).value(),
                password: self.cc_pass.read(cx).value(),
            };
            self.store.save_cookiecloud(&cc)?;
            let jackett = JackettForm {
                enabled: self.jackett_enabled,
                url: self.jackett_url.read(cx).value(),
                api_key: self.jackett_apikey.read(cx).value(),
                concurrency: self.jackett_concurrency.read(cx).value(),
                timeout: self.jackett_timeout.read(cx).value(),
            };
            self.store.save_jackett(&jackett)?;
            let level = self.log_level.read(cx).value();
            let level = if level.trim().is_empty() { "INFO".to_string() } else { level };
            let file = self.log_file.read(cx).value();
            let file = if file.trim().is_empty() { "logs/reseed.log".to_string() } else { file };
            self.store.save_log(&level, &file)?;
            Ok(())
        })();
        let sched_changed = self.sched_enabled != sched_was_enabled;
        match result {
            Ok(()) => {
                self.status = Some(if sched_changed {
                    (
                        "已保存。注意：定时调度开关变更需重启程序后生效".into(),
                        true,
                    )
                } else {
                    ("已保存，下一轮辅种自动生效".into(), true)
                })
            }
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
            .flex_1()
            .min_h_0()
            .w_full()
            .gap_4()
            // 标签栏（下划线激活态）
            .child(
                div()
                    .flex()
                    .gap_5()
                    .border_b_1()
                    .border_color(theme::DIVIDER)
                    .children(vec![
                        Self::tab_button("下载器", tab == Tab::Downloader, "tab-dl", Tab::Downloader, cx),
                        Self::tab_button("站点", tab == Tab::Sites, "tab-sites", Tab::Sites, cx),
                        Self::tab_button("调度与全局", tab == Tab::Scheduler, "tab-sched", Tab::Scheduler, cx),
                        Self::tab_button("通知与集成", tab == Tab::Notify, "tab-notify", Tab::Notify, cx),
                    ]),
            )
            // 内容区（滚动）
            .child(
                div()
                    .id("settings-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .py_5()
                            .rounded_lg()
                            .bg(theme::SURFACE)
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
                                    Self::field_row("添加种子标签（tag）", &self.dl_tag, None),
                                    Self::toggle_row(
                                        "跳过哈希校验",
                                        "添加种子时 skip_checking（推荐开启）",
                                        self.dl_skip_hash_check,
                                        "dl-skip-hash",
                                        cx.listener(|this, _ev, _w, cx| {
                                            this.dl_skip_hash_check = !this.dl_skip_hash_check;
                                            cx.notify();
                                        }),
                                    ),
                                    Self::toggle_row(
                                        "添加后自动开始",
                                        "关闭则添加为暂停状态",
                                        self.dl_auto_start,
                                        "dl-auto-start",
                                        cx.listener(|this, _ev, _w, cx| {
                                            this.dl_auto_start = !this.dl_auto_start;
                                            cx.notify();
                                        }),
                                    ),
                                ],
                                Tab::Sites => {
                                    let mut rows: Vec<AnyElement> = self.sites.iter().enumerate().map(|(idx, row)| {
                                        let enabled = row.form.enabled;
                                        let (dot, color) = if enabled {
                                            ("●", theme::GREEN)
                                        } else {
                                            ("○", theme::TEXT_FAINT)
                                        };
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap_3()
                                            .px_5()
                                            .py_4()
                                            .rounded_md()
                                            .bg(theme::INPUT_BG)
                                            .child(
                                                div().flex().justify_between().items_center()
                                                    .child(
                                                        div().flex().items_center().gap_2()
                                                            .child(div().text_xs().text_color(color).child(dot.to_string()))
                                                            .child(div().text_sm().text_color(theme::TEXT_DIM).child(format!("站点 #{}", idx + 1))),
                                                    )
                                            .child(
                                                div().flex().gap_2()
                                                    .child(
                                                        div()
                                                            .id(("site-toggle", idx))
                                                            .rounded_md()
                                                            .hover(|s| s.cursor_pointer())
                                                            .child(Self::switch(enabled))
                                                            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _ev, _w, cx| {
                                                                if let Some(r) = this.sites.get_mut(idx) {
                                                                    r.form.enabled = !r.form.enabled;
                                                                }
                                                                cx.notify();
                                                            })),
                                                    )
                                                    .child(
                                                        div()
                                                            .id(("site-del", idx))
                                                            .px_2()
                                                            .py_1()
                                                            .rounded_md()
                                                            .text_xs()
                                                            .text_color(theme::TEXT_FAINT)
                                                            .hover(|s| s.cursor_pointer().text_color(theme::RED).bg(theme::BTN_GHOST_HOVER))
                                                            .child("删除")
                                                            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _ev, _w, cx| {
                                                                if this.sites.len() > idx {
                                                                    this.sites.remove(idx);
                                                                }
                                                                cx.notify();
                                                            })),
                                                    ),
                                            ),
                                            )
                                            .child(div().flex().gap_3()
                                                .child(div().flex_1().child(Self::field_row("名称", &row.name, None)))
                                                .child(div().flex_1().child(Self::field_row("主页", &row.url, None))))
                                            .child(Self::field_row("Passkey", &row.passkey, None))
                                            .into_any_element()
                                    }).collect();
                                    // 新增站点（ghost 文字按钮）
                                    rows.push(
                                        div()
                                            .id("site-add")
                                            .px_3()
                                            .py_2()
                                            .rounded_md()
                                            .text_sm()
                                            .text_color(theme::TEXT_DIM)
                                            .hover(|s| s.cursor_pointer().text_color(theme::TEXT).bg(theme::BTN_GHOST_HOVER))
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
                                    Self::toggle_row(
                                        "启用定时调度",
                                        "关闭时仅托盘/按钮手动触发辅种",
                                        self.sched_enabled,
                                        "sched-toggle",
                                        cx.listener(|this, _ev, _w, cx| {
                                            this.sched_enabled = !this.sched_enabled;
                                            cx.notify();
                                        }),
                                    ),
                                    Self::field_row("调度间隔（分钟）", &self.sched_interval, None),
                                    Self::toggle_row(
                                        "启动即执行首轮",
                                        "关闭则等待一个完整间隔后再跑",
                                        self.sched_run_on_start,
                                        "ros-toggle",
                                        cx.listener(|this, _ev, _w, cx| {
                                            this.sched_run_on_start = !this.sched_run_on_start;
                                            cx.notify();
                                        }),
                                    ),
                                    div().h_px().bg(theme::DIVIDER).into_any_element(),
                                    Self::field_row("站点查询并发数", &self.glob_concurrency, None),
                                    Self::field_row("API 批量大小", &self.glob_batch, None),
                                    Self::field_row("辅种模式（pieces_hash / jackett / both）", &self.glob_mode, None),
                                    div().h_px().bg(theme::DIVIDER).into_any_element(),
                                    Self::field_row("日志级别（DEBUG / INFO / WARN / ERROR）", &self.log_level,
                                        Some("重启程序后生效")),
                                    Self::field_row("日志文件路径", &self.log_file, None),
                                ],
                                Tab::Notify => vec![
                                    // 飞书
                                    div().text_xs().text_color(theme::TEXT_FAINT).child("飞书通知").into_any_element(),
                                    Self::field_row("Webhook URL", &self.feishu_webhook,
                                        Some("留空则不推送")),
                                    Self::field_row("签名校验 Secret", &self.feishu_secret, None),
                                    div().h_px().bg(theme::DIVIDER).into_any_element(),
                                    // CookieCloud
                                    div().text_xs().text_color(theme::TEXT_FAINT).child("COOKIECLOUD 同步").into_any_element(),
                                    Self::toggle_row(
                                        "启用 CookieCloud",
                                        "每轮辅种开始前自动同步站点 Cookie",
                                        self.cc_enabled,
                                        "cc-toggle",
                                        cx.listener(|this, _ev, _w, cx| {
                                            this.cc_enabled = !this.cc_enabled;
                                            cx.notify();
                                        }),
                                    ),
                                    Self::field_row("服务地址", &self.cc_url, None),
                                    Self::field_row("UUID", &self.cc_uuid, None),
                                    Self::field_row("解密密码", &self.cc_pass, None),
                                    div().h_px().bg(theme::DIVIDER).into_any_element(),
                                    // Jackett
                                    div().text_xs().text_color(theme::TEXT_FAINT).child("JACKETT（jackett / both 模式）").into_any_element(),
                                    Self::toggle_row(
                                        "启用 Jackett",
                                        "辅种模式为 jackett / both 时生效",
                                        self.jackett_enabled,
                                        "jackett-toggle",
                                        cx.listener(|this, _ev, _w, cx| {
                                            this.jackett_enabled = !this.jackett_enabled;
                                            cx.notify();
                                        }),
                                    ),
                                    Self::field_row("服务地址", &self.jackett_url, None),
                                    Self::field_row("API Key", &self.jackett_apikey, None),
                                    div().flex().gap_3()
                                        .child(div().w_32().child(Self::field_row("并发数", &self.jackett_concurrency, None)))
                                        .child(div().w_32().child(Self::field_row("超时（秒）", &self.jackett_timeout, None)))
                                        .into_any_element(),
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
                    .pt_3()
                    .border_t_1()
                    .border_color(theme::DIVIDER)
                    .children(status.map(|(msg, ok)| {
                        div()
                            .text_sm()
                            .text_color(if ok { theme::GREEN } else { theme::RED })
                            .child(msg)
                    }))
                    .child(
                        div()
                            .id("settings-save")
                            .px_5()
                            .py_1p5()
                            .rounded_md()
                            .bg(theme::BTN_PRIMARY)
                            .text_color(theme::TEXT_ON_PRIMARY)
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
