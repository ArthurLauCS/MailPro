//! 设计令牌：StyleKit 的 Linear 风格。冷调中性色，近黑 / 冷白，1px 细线，不用阴影分层；强调色取应用图标的粉色。
//! 颜色写进组件库的主题，各处照旧用 `cx.theme()` 取。字号：正文和列表行 [`BODY`]，附注 text_xs（12），
//! 栏标题 text_sm（14），分区标题 text_base（16），邮件主题 text_xl（20）；字重只用常规和 SEMIBOLD。

use gpui::{Hsla, Pixels, Window, WindowAppearance, px, rgb};
use gpui_component::theme::{Theme, ThemeMode};
use std::path::PathBuf;

/// 字体按角色分，中日韩字符由 DirectWrite 自动回退到微软雅黑 UI。UI：控件、菜单、标签；
/// DISPLAY：邮件主题和发件人；PROSE：邮件正文（写信框，也是正文查看器的默认字体）；MONO：时间、数量、地址
pub const UI: &str = "Segoe UI Variable Text";
pub const DISPLAY: &str = "Noto Serif SC";
pub const PROSE: &str = "LXGW Neo XiHei";
pub const MONO: &str = "Cascadia Mono";
/// DISPLAY、PROSE 两套随程序打包（scripts/fonts.ps1 下载），UI、MONO 是系统自带的
pub const PROSE_FILE: &str = "LXGWNeoXiHei.ttf";
pub const FONT_FILES: [&str; 2] = ["NotoSerifSC-VF.ttf", PROSE_FILE];
pub const BODY: Pixels = px(13.);

pub const R_MD: Pixels = px(6.);
pub const R_LG: Pixels = px(8.);

struct Palette {
    /// 底色三层：内容区、面板（左栏），悬停 / 选中 / 输入框 / 菜单用的 raised
    bg: Hsla,
    panel: Hsla,
    raised: Hsla,
    border: Hsla,
    text: Hsla,
    muted: Hsla,
    /// 文字、线条、焦点框和主按钮底色；on_accent 是压在它上面的字
    accent: Hsla,
    on_accent: Hsla,
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
            bg: h(0x0d0e10),
            panel: h(0x141517),
            raised: h(0x1b1c1f),
            border: h(0x27282c),
            text: h(0xecedee),
            muted: h(0x8a8f98),
            accent: h(0xf07aae),
            on_accent: h(0x1a1a1a),
            green: h(0x3dbd73),
            orange: h(0xf2994a),
            red: h(0xeb5757),
            on_danger: h(0x0d0e10),
            backdrop: h(0x000000).opacity(0.6),
        }
    }

    fn light() -> Palette {
        Palette {
            bg: h(0xfbfbfb),
            panel: h(0xf4f4f5),
            raised: h(0xeaeaec),
            border: h(0xdfdfe2),
            text: h(0x18191b),
            muted: h(0x62666d),
            accent: h(0xb32e6a),
            on_accent: h(0xffffff),
            green: h(0x1d7f48),
            orange: h(0xb0590e),
            red: h(0xcc3d3d),
            on_danger: h(0xffffff),
            backdrop: h(0x18191b).opacity(0.3),
        }
    }
}

/// 读打包的字体文件：安装版在 exe 旁的 fonts 目录，开发时在 crates/app/assets/fonts
pub fn font_file(name: &str) -> Option<Vec<u8>> {
    let exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("fonts")));
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/fonts");
    exe.iter().chain([&dev]).find_map(|dir| std::fs::read(dir.join(name)).ok())
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
        k.ring = p.accent.opacity(0.6);
        k.caret = p.text;
        k.selection = p.accent.opacity(if dark { 0.3 } else { 0.18 });
        k.primary = p.accent;
        k.primary_hover = p.accent;
        k.primary_active = p.accent;
        k.primary_foreground = p.on_accent;
        // 主按钮的颜色单独存一份，不跟 primary 走
        k.button_primary = p.accent;
        k.button_primary_hover = p.accent;
        k.button_primary_active = p.accent;
        k.button_primary_foreground = p.on_accent;
        k.secondary = p.raised;
        k.secondary_hover = p.raised;
        k.secondary_active = p.raised;
        k.secondary_foreground = p.text;
        k.muted = p.raised;
        k.muted_foreground = p.muted;
        // 下拉菜单的悬停和选中项
        k.accent = p.raised;
        k.accent_foreground = p.text;
        k.popover = p.raised;
        k.popover_foreground = p.text;
        k.sidebar = p.panel;
        k.sidebar_foreground = p.text;
        k.list = p.bg;
        k.list_hover = p.raised;
        k.list_active = p.raised;
        k.list_active_border = p.accent;
        k.link = p.accent;
        k.link_hover = p.accent;
        k.link_active = p.accent;
        k.success = p.green;
        k.warning = p.orange;
        k.danger = p.red;
        k.danger_foreground = p.on_danger;
        k.scrollbar = p.bg.opacity(0.);
        k.scrollbar_thumb = p.border;
        k.scrollbar_thumb_hover = p.muted;
        k.overlay = p.backdrop;
        k.window_border = p.border;
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
