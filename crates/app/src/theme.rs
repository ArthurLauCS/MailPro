//! 设计令牌，和 PushRight 同一套：暖调的象牙白与炭黑、衬线标题；强调色取应用图标的粉色。
//! 颜色写进组件库的主题，各处照旧用 `cx.theme()` 取；字号用组件库按 16px 算的 text_xs / text_sm 等。

use gpui::{Hsla, Pixels, Window, WindowAppearance, px, rgb};
use gpui_component::theme::{Theme, ThemeMode};

/// 三个字族各管一个区域：UI 界面控件；DISPLAY 标题和名字：思源宋体；
/// PROSE 给人读的整段话（说明、空状态）：霞鹜文楷。中日韩字符由 DirectWrite 自动回退到系统字体
pub const UI: &str = "Segoe UI Variable Text";
pub const DISPLAY: &str = "Noto Serif SC";
pub const PROSE: &str = "LXGW WenKai";

pub const R_MD: Pixels = px(8.);
pub const R_LG: Pixels = px(14.);

struct Palette {
    /// 底色由深到浅三层：内容区、面板（左栏）、浮起的控件。全部带一点暖调
    bg: Hsla,
    panel: Hsla,
    raised: Hsla,
    border: Hsla,
    border_strong: Hsla,
    text: Hsla,
    muted: Hsla,
    hover: Hsla,
    /// 强调色按用途分两个：accent 当文字、线条、焦点框用，深色主题里取亮一档；
    /// accent_fill 是上面压着字的底色（主按钮、未读圆点），白字要看得清，取图标渐变里深的那档
    accent: Hsla,
    accent_fill: Hsla,
    /// 选中行的底色
    accent_soft: Hsla,
    green: Hsla,
    orange: Hsla,
    red: Hsla,
    on_danger: Hsla,
    backdrop: Hsla,
}

fn h(hex: u32) -> Hsla {
    rgb(hex).into()
}

impl Palette {
    fn dark() -> Palette {
        Palette {
            bg: h(0x1f1e1d),
            panel: h(0x262624),
            raised: h(0x30302e),
            border: h(0x3a3936),
            border_strong: h(0x5a5852),
            text: h(0xf0eee6),
            muted: h(0xa9a69c),
            hover: h(0xf0eee6).opacity(0.06),
            accent: h(0xf472b6),
            accent_fill: h(0xdb2777),
            accent_soft: h(0xec4899).opacity(0.12),
            green: h(0x91b876),
            orange: h(0xfb923c),
            red: h(0xef7b88),
            on_danger: h(0x1f1e1d),
            backdrop: h(0x141312).opacity(0.55),
        }
    }

    fn light() -> Palette {
        Palette {
            bg: h(0xf4f2ea),
            panel: h(0xebe8de),
            raised: h(0xe0dccf),
            border: h(0xd4d0c2),
            border_strong: h(0xada999),
            text: h(0x1f1e1c),
            muted: h(0x5e5d57),
            hover: h(0x141413).opacity(0.05),
            accent: h(0xbe185d),
            accent_fill: h(0xdb2777),
            accent_soft: h(0xec4899).opacity(0.12),
            green: h(0x3f6b2f),
            orange: h(0xb45309),
            red: h(0xa82835),
            on_danger: h(0xfaf9f5),
            backdrop: h(0x28241c).opacity(0.32),
        }
    }
}

pub fn system_dark(window: &Window) -> bool {
    matches!(window.appearance(), WindowAppearance::Dark | WindowAppearance::VibrantDark)
}

/// 按深浅色重设组件库的颜色，输入框、菜单、对话框、滚动条和窗口标题栏都用同一套
pub fn apply(dark: bool, window: &mut Window, cx: &mut gpui::App) {
    let p = if dark { Palette::dark() } else { Palette::light() };
    Theme::change(if dark { ThemeMode::Dark } else { ThemeMode::Light }, Some(window), cx);
    // 组件库的颜色存两份（colors 和勾选框、悬停提示读的 tokens），用 update 改 colors，tokens 跟着改
    Theme::update(cx, |theme| {
        theme.font_family = UI.into();
        theme.radius = R_MD;
        theme.radius_lg = R_LG;
        // 滚动条常驻，长列表一眼看出能滚、滚到了哪
        theme.scrollbar_mode = gpui_component::scroll::ScrollbarMode::Always;
        let k = &mut theme.colors;
        k.background = p.bg;
        k.foreground = p.text;
        k.border = p.border;
        k.input = p.border;
        k.ring = p.accent;
        k.caret = p.text;
        k.selection = p.accent_fill.opacity(if dark { 0.3 } else { 0.25 });
        k.primary = p.accent_fill;
        k.primary_hover = p.accent_fill;
        k.primary_active = p.accent_fill;
        k.primary_foreground = h(0xffffff);
        k.secondary = p.raised;
        k.secondary_hover = p.raised;
        k.secondary_active = p.raised;
        k.secondary_foreground = p.text;
        k.muted = p.raised;
        k.muted_foreground = p.muted;
        // 下拉菜单的悬停和选中项：上面是正文色的字，用淡的那档
        k.accent = p.accent_soft;
        k.accent_foreground = p.text;
        k.popover = p.raised;
        k.popover_foreground = p.text;
        k.sidebar = p.panel;
        k.sidebar_foreground = p.text;
        k.list = p.bg;
        k.list_hover = p.hover;
        k.list_active = p.accent_soft;
        k.list_active_border = p.accent;
        // 强调色当文字用的那档：链接按钮，以及选中的文件夹
        k.link = p.accent;
        k.link_hover = p.accent;
        k.link_active = p.accent;
        k.success = p.green;
        k.warning = p.orange;
        k.danger = p.red;
        k.danger_foreground = p.on_danger;
        k.scrollbar = p.bg.opacity(0.);
        k.scrollbar_thumb = p.border;
        k.scrollbar_thumb_hover = p.border_strong;
        k.overlay = p.backdrop;
        k.window_border = p.border_strong;
    });
    title_bar(dark, window);
}

/// 系统画的标题栏只按系统的深浅色走，这里改成跟应用主题一致
fn title_bar(dark: bool, window: &Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
    let Ok(RawWindowHandle::Win32(handle)) = HasWindowHandle::window_handle(window).map(|h| h.as_raw()) else { return };
    let on = windows::core::BOOL::from(dark);
    // 设置失败只是标题栏颜色不对，不影响使用
    let _ = unsafe { DwmSetWindowAttribute(HWND(handle.hwnd.get() as _), DWMWA_USE_IMMERSIVE_DARK_MODE, &on as *const _ as _, size_of_val(&on) as u32) };
}
