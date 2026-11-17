//! Windows 系统托盘：图标 + 右键菜单（立即辅种 / 退出）。
//!
//! 独立线程跑 Windows 消息泵（tray-icon 后端要求），事件经 mpsc 回主进程。

use std::time::Duration;

#[derive(Clone)]
pub enum TrayCommand {
    Quit,
}

/// 托盘主循环：阻塞直到收到退出。
pub fn run_tray(
    rx: std::sync::mpsc::Receiver<TrayCommand>,
    trigger: tokio::sync::mpsc::Sender<()>,
    stop: tokio::sync::watch::Sender<bool>,
) -> Result<(), Box<dyn std::error::Error>> {
    use tray_icon::menu::{Menu, MenuItem};
    use tray_icon::TrayIconBuilder;

    let run_now = MenuItem::new("立即辅种", true, None);
    let quit = MenuItem::new("退出", true, None);
    let menu = Menu::new();
    menu.append(&run_now)?;
    menu.append(&quit)?;

    let icon = tray_icon()?;
    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("ReseedHound RS - 辅种引擎")
        .with_icon(icon)
        .build()?;

    loop {
        if let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
            if event.id == run_now.id() {
                let _ = trigger.try_send(());
            } else if event.id == quit.id() {
                let _ = stop.send(true);
                break;
            }
        }
        if let Ok(cmd) = rx.try_recv() {
            match cmd {
                TrayCommand::Quit => {
                    let _ = stop.send(true);
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

/// 程序化生成托盘图标：圆角深空底 + 中心种子形 + 边缘光晕（与 GUI 品牌色呼应）。
fn tray_icon() -> Result<tray_icon::Icon, Box<dyn std::error::Error>> {
    const W: usize = 32;
    const H: usize = 32;
    let mut rgba = vec![0u8; W * H * 4];
    for y in 0..H {
        for x in 0..W {
            let i = (y * W + x) * 4;
            // 归一化坐标 [-1, 1]
            let nx = (x as f64 - 15.5) / 15.5;
            let ny = (y as f64 - 15.5) / 15.5;
            let dist = (nx * nx + ny * ny).sqrt();
            // 圆底：半径 1.0 之外透明（抗锯齿 1 像素过渡）
            if dist > 1.05 {
                continue;
            }
            let alpha = ((1.05 - dist) / 0.1).clamp(0.0, 1.0);
            // 深空渐变底：左上深藏青 → 右下深蓝
            let t = (nx + ny + 2.0) / 4.0;
            let (mut r, mut g, mut b) = (
                16.0 + t * 12.0,
                20.0 + t * 16.0,
                40.0 + t * 40.0,
            );
            // 中心种子形（十字加圆心）
            let cx = nx.abs();
            let cy = ny.abs();
            let in_seed = (cx <= 0.14 && cy <= 0.62) || (cy <= 0.14 && cx <= 0.62) || dist < 0.16;
            if in_seed {
                // 亮青蓝，带轻微径向亮度
                let glow = 1.0 - dist * 0.4;
                r = 120.0 + glow * 20.0;
                g = 190.0 + glow * 30.0;
                b = 250.0 + glow * 5.0;
            }
            rgba[i] = r as u8;
            rgba[i + 1] = g as u8;
            rgba[i + 2] = b as u8;
            rgba[i + 3] = (alpha * 255.0) as u8;
        }
    }
    Ok(tray_icon::Icon::from_rgba(rgba, W as u32, H as u32)?)
}
