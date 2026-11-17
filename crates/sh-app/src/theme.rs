//! 设计系统：统一配色 / 间距 / 圆角，深色主题（Task 审美升级）。
//!
//! 色板取自 Material Design 暗色基调 + 自定义品牌色（藏青-青蓝渐变系），
//! 与托盘图标（深蓝底 + 亮蓝种子）视觉呼应。

use gpui::Rgba;

/// const 构造 Rgba（gpui::rgb 不是 const fn）。
const fn c(hex: u32) -> Rgba {
    Rgba {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

// ---------- 品牌色 ----------

/// 主品牌色：亮蓝紫（标题 / 顶栏 / 强调）
pub const ACCENT: Rgba = c(0x7c9cf5);

/// 顶栏底部渐变的亮端（预留渐变装饰用）
#[allow(dead_code)]
pub const ACCENT_BRIGHT: Rgba = c(0x4fc3f7);

/// 页面背景
pub const BG: Rgba = c(0x10141f);

/// 卡片 / 面板背景
pub const SURFACE: Rgba = c(0x1a1f2e);

/// 卡片 hover 背景
pub const SURFACE_HOVER: Rgba = c(0x222838);

/// 输入框背景
pub const INPUT_BG: Rgba = c(0x0d1117);

/// 边框
pub const BORDER: Rgba = c(0x2c3447);

/// 主文本
pub const TEXT: Rgba = c(0xe8eaf0);

/// 次要文本（标签 / 说明）
pub const TEXT_DIM: Rgba = c(0x8b93a7);

// ---------- 语义色 ----------

pub const GREEN: Rgba = c(0x4ade80);
pub const RED: Rgba = c(0xf87171);
pub const YELLOW: Rgba = c(0xfbbf24);
pub const BLUE: Rgba = c(0x60a5fa);

/// 链接 / 交互强调
pub const LINK: Rgba = c(0x93c5fd);

// ---------- 派生 ----------

/// 半透明遮罩（弹层背景，预留）
#[allow(dead_code)]
pub const OVERLAY: Rgba = Rgba { r: 0.05, g: 0.05, b: 0.09, a: 0.55 };

/// 主要按钮背景（蓝紫渐变的中间值）
pub const BTN_PRIMARY: Rgba = c(0x3b82f6);

pub const BTN_PRIMARY_HOVER: Rgba = c(0x2563eb);

/// 次要按钮（描边）
pub const BTN_GHOST_HOVER: Rgba = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.125 };

/// 禁用
pub const DISABLED: Rgba = c(0x4b5563);

// ---------- 状态灯四档（站点明细） ----------

pub fn status_dot(failed: u64, succeeded: u64, matched: u64) -> (&'static str, Rgba) {
    if failed > 0 {
        ("●", RED)
    } else if succeeded > 0 {
        ("●", GREEN)
    } else if matched > 0 {
        ("●", YELLOW)
    } else {
        ("○", TEXT_DIM)
    }
}

/// 状态条文字与颜色（顶栏运行状态）。
pub fn status_badge(running: bool, scheduler_on: bool) -> (&'static str, Rgba) {
    if running {
        ("辅种运行中", BLUE)
    } else if scheduler_on {
        ("调度待机", GREEN)
    } else {
        ("手动模式", TEXT_DIM)
    }
}
