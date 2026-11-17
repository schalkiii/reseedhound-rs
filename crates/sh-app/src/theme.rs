//! 设计系统：中性石墨基底 + 单一哑光强调色（专业工具风，类 Zed/Linear）。
//!
//! 原则：
//! - 背景分层靠明度微差（BG < SURFACE < ELEVATED），不靠边框堆砌
//! - 唯一品牌强调色（雾蓝），只用于主按钮与激活态；语义色仅状态灯使用
//! - 文字层级：TEXT / TEXT_DIM / TEXT_FAINT 三档

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

// ---------- 中性基底（石墨系） ----------

/// 页面背景
pub const BG: Rgba = c(0x141518);

/// 卡片 / 面板背景
pub const SURFACE: Rgba = c(0x1b1d21);

/// 输入框背景（比面板更深一档，凹陷感）
pub const INPUT_BG: Rgba = c(0x111316);

/// 分隔线（比边框更克制）
pub const DIVIDER: Rgba = c(0x232629);

// ---------- 文字 ----------

/// 主文本
pub const TEXT: Rgba = c(0xdcdde1);

/// 次要文本（标签 / 说明）
pub const TEXT_DIM: Rgba = c(0x8b8f98);

/// 弱文本（占位 / 脚注）
pub const TEXT_FAINT: Rgba = c(0x5f636b);

// ---------- 品牌强调（唯一彩色，哑光雾蓝） ----------

pub const ACCENT: Rgba = c(0x6a9bd8);

pub const BTN_PRIMARY: Rgba = c(0x3d6db3);

pub const BTN_PRIMARY_HOVER: Rgba = c(0x4a7cc6);

// ---------- 语义色（低饱和，仅状态） ----------

pub const GREEN: Rgba = c(0x5da970);
pub const RED: Rgba = c(0xc96a6a);
pub const YELLOW: Rgba = c(0xbfa14e);
pub const BLUE: Rgba = c(0x6a9bd8);

// ---------- 派生 ----------

/// 次要按钮 hover
pub const BTN_GHOST_HOVER: Rgba = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.08 };

/// 禁用
pub const DISABLED: Rgba = c(0x3b3e44);

/// 主按钮上的文字
pub const TEXT_ON_PRIMARY: Rgba = c(0xf2f5f9);

/// 全透明（非激活 Tab 下划线占位，保持基线对齐）
pub const TRANSPARENT: Rgba = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };

// ---------- 状态灯（站点明细，返回字符为空——用色点绘制） ----------

pub fn status_dot(failed: u64, succeeded: u64, matched: u64) -> (&'static str, Rgba) {
    if failed > 0 {
        ("●", RED)
    } else if succeeded > 0 {
        ("●", GREEN)
    } else if matched > 0 {
        ("●", YELLOW)
    } else {
        ("○", TEXT_FAINT)
    }
}

/// 状态条文字与颜色（顶栏运行状态）。
pub fn status_badge(running: bool, scheduler_on: bool) -> (&'static str, Rgba) {
    if running {
        ("运行中", BLUE)
    } else if scheduler_on {
        ("调度待机", GREEN)
    } else {
        ("手动", TEXT_DIM)
    }
}
