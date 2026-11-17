//! 首启配置向导：无 config.yaml/sites.yaml 时引导用户完成初始配置。
//!
//! 流程（三步）：
//! 1. 欢迎 + 下载器（qBittorrent/Transmission 地址、账号、种子目录）
//! 2. 站点（名称 / url / passkey，至少一个，可跳过稍后在设置页补）
//! 3. 完成 → 写盘 → 通知主进程启动调度与仪表盘
//!
//! 设计原则：不退出、不弹系统对话框；全部在 GPUI 窗口内完成。

use std::sync::Arc;

use gpui::*;

use crate::config_store::{ConfigStore, DownloaderForm, SiteForm};
use crate::text_input::TextInput;
use crate::theme;

/// 向导完成事件（RootView 订阅后切换仪表盘）。
pub struct WizardDone;

impl EventEmitter<WizardDone> for SetupWizard {}

/// 向导完成回调类型。
pub type WizardDoneCallback = Box<dyn Fn(&mut Window, &mut Context<SetupWizard>) + 'static>;

pub struct SetupWizard {
    step: u8,
    store: Arc<ConfigStore>,
    // 下载器字段
    dl_type: Entity<TextInput>,
    dl_host: Entity<TextInput>,
    dl_port: Entity<TextInput>,
    dl_user: Entity<TextInput>,
    dl_pass: Entity<TextInput>,
    dl_dir: Entity<TextInput>,
    // 站点字段（首个站点）
    site_name: Entity<TextInput>,
    site_url: Entity<TextInput>,
    site_passkey: Entity<TextInput>,
    /// 错误提示（保存失败时显示）
    error: Option<String>,
    /// 完成回调（主流程切换到仪表盘）：签名对齐 EventEmitter 用法，
    /// 保存成功后 emit WizardDone。
    on_done: WizardDoneCallback,
}

impl SetupWizard {
    pub fn new(
        store: Arc<ConfigStore>,
        on_done: WizardDoneCallback,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = |placeholder: &str, initial: &str, cx: &mut Context<Self>| {
            cx.new(|cx| TextInput::new(cx, placeholder, initial, false))
        };
        let masked = |placeholder: &str, initial: &str, cx: &mut Context<Self>| {
            cx.new(|cx| TextInput::new(cx, placeholder, initial, true))
        };
        Self {
            step: 1,
            store,
            dl_type: input("qbittorrent", "qbittorrent", cx),
            dl_host: input("127.0.0.1", "127.0.0.1", cx),
            dl_port: input("8080", "8080", cx),
            dl_user: input("用户名", "", cx),
            dl_pass: masked("密码", "", cx),
            dl_dir: input("种子文件保存目录（如 D:/torrents）", "", cx),
            site_name: input("站点名（如 mteam）", "", cx),
            site_url: input("站点主页（如 https://kp.m-team.cc）", "", cx),
            site_passkey: masked("Passkey（站点控制面板获取）", "", cx),
            error: None,
            on_done,
        }
    }

    fn field_row(
        label: &str,
        input: &Entity<TextInput>,
        hint: Option<&str>,
    ) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_sm().text_color(theme::TEXT_DIM).child(label.to_string()))
            .child(input.clone())
            .children(hint.map(|h| {
                div()
                    .text_xs()
                    .text_color(theme::TEXT_DIM)
                    .child(h.to_string())
            }))
            .into_any_element()
    }

    fn save_and_finish(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // 1. 初始化模板（若文件不存在则从 example 生成）
        if let Err(e) = self.store.init_from_example() {
            self.error = Some(format!("模板初始化失败: {e}"));
            cx.notify();
            return;
        }
        // 2. 保存下载器
        let form = DownloaderForm {
            dl_type: self.dl_type.read(cx).value(),
            host: self.dl_host.read(cx).value(),
            port: self.dl_port.read(cx).value(),
            username: self.dl_user.read(cx).value(),
            password: self.dl_pass.read(cx).value(),
            torrent_dir: self.dl_dir.read(cx).value(),
            tag: String::new(),
            skip_hash_check: None,
            auto_start: None,
        };
        if let Err(e) = self.store.save_downloader(&form) {
            self.error = Some(format!("下载器配置保存失败: {e}"));
            cx.notify();
            return;
        }
        // 3. 保存站点（用户填了才写）
        let name = self.site_name.read(cx).value();
        if !name.trim().is_empty() {
            let site = SiteForm {
                name: name.trim().to_string(),
                url: self.site_url.read(cx).value().trim().to_string(),
                passkey: self.site_passkey.read(cx).value().trim().to_string(),
                api_url: String::new(),
                enabled: true,
            };
            if let Err(e) = self.store.save_sites(&[site]) {
                self.error = Some(format!("站点配置保存失败: {e}"));
                cx.notify();
                return;
            }
        }
        (self.on_done)(_window, cx);
        cx.emit(WizardDone);
    }
}

impl Render for SetupWizard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let step = self.step;
        let error = self.error.clone();

        // 通用容器
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::BG)
            .p_10()
            .gap_6()
            .child(
                // 标题 + 步骤指示
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_2xl()
                            .text_color(theme::TEXT)
                            .child("欢迎使用 ReseedHound RS"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme::TEXT_DIM)
                            .child(match step {
                                1 => "第 1 步 / 共 2 步 · 下载器连接",
                                2 => "第 2 步 / 共 2 步 · PT 站点",
                                _ => "完成",
                            }),
                    ),
            )
            .child(
                // 表单卡片（无外框，靠明度分层）
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .p_6()
                    .rounded_lg()
                    .bg(theme::SURFACE)
                    .children(match step {
                        1 => vec![
                            Self::field_row("下载器类型（qbittorrent / transmission）", &self.dl_type, None),
                            div().flex().gap_3()
                                .child(div().flex_1().child(Self::field_row("地址", &self.dl_host, None)))
                                .child(div().w_32().child(Self::field_row("端口", &self.dl_port, None)))
                                .into_any_element(),
                            Self::field_row("用户名", &self.dl_user, None),
                            Self::field_row("密码", &self.dl_pass, None),
                            Self::field_row("种子目录（辅种 .torrent 文件保存位置）", &self.dl_dir,
                                Some("qBittorrent: 设置 → 下载 → 监控的 .torrent 文件目录")),
                        ],
                        _ => vec![
                            Self::field_row("站点名", &self.site_name, Some("展示名，也是 --site 过滤名")),
                            Self::field_row("站点主页", &self.site_url, Some("用于按 tracker 域名排除同站种子")),
                            Self::field_row("Passkey", &self.site_passkey,
                                Some("站点控制面板 → 密钥/Passkey；留空跳过，稍后可在设置页补")),
                        ],
                    }),
            )
            .children(error.map(|e| {
                div()
                    .px_4().py_2()
                    .rounded_md()
                    .bg(rgb(0x3a2424))
                    .text_color(theme::RED)
                    .text_sm()
                    .child(e)
            }))
            .child(
                // 底部按钮
                div()
                    .flex()
                    .justify_between()
                    .mt_auto()
                    .child(
                        // 上一步（第 1 步时禁用视觉）
                        div()
                            .id("wiz-back")
                            .px_4().py_1p5()
                            .rounded_md()
                            .text_sm()
                            .text_color(theme::TEXT_DIM)
                            .opacity(if step > 1 { 1.0 } else { 0.3 })
                            .hover(|s| if step > 1 { s.cursor_pointer().bg(theme::BTN_GHOST_HOVER).text_color(theme::TEXT) } else { s })
                            .child("上一步")
                            .on_mouse_down(MouseButton::Left, cx.listener(|this, _ev, _w, cx| {
                                if this.step > 1 {
                                    this.step -= 1;
                                    this.error = None;
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        div().flex().gap_3()
                            .child(if step == 2 {
                                // 跳过站点（ghost 文字按钮）
                                div()
                                    .id("wiz-skip")
                                    .px_4().py_1p5()
                                    .rounded_md()
                                    .text_sm()
                                    .text_color(theme::TEXT_DIM)
                                    .hover(|s| s.cursor_pointer().bg(theme::BTN_GHOST_HOVER).text_color(theme::TEXT))
                                    .child("跳过站点，稍后配置")
                                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _ev, w, cx| {
                                        this.site_name.update(cx, |i, _c| { i.content = "".into(); });
                                        this.save_and_finish(w, cx);
                                    }))
                                    .into_any_element()
                            } else {
                                div().into_any_element()
                            })
                            .child(
                                div()
                                    .id("wiz-next")
                                    .px_5().py_1p5()
                                    .rounded_md()
                                    .bg(theme::BTN_PRIMARY)
                                    .text_color(theme::TEXT_ON_PRIMARY)
                                    .text_sm()
                                    .hover(|s| s.cursor_pointer().bg(theme::BTN_PRIMARY_HOVER))
                                    .child(if step == 1 { "下一步".to_string() } else { "完成配置".to_string() })
                                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _ev, w, cx| {
                                        if this.step == 1 {
                                            this.step = 2;
                                            this.error = None;
                                            cx.notify();
                                        } else {
                                            this.save_and_finish(w, cx);
                                        }
                                    })),
                            ),
                    ),
            )
    }
}
