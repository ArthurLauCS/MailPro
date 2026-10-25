#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod browser;
mod i18n;
mod view;
mod webview;

use gpui::prelude::*;
use gpui::{App, Bounds, TitlebarOptions, WindowBackgroundAppearance, WindowBounds, WindowOptions, px, size, transparent_black};
use gpui_component::Root;
use mailpro_core::store::{self, Store};
use mailpro_core::{cred, message};

fn main() {
    // 卸载程序调用：删掉凭据管理器里本程序存的密码，数据目录由卸载程序自己删
    if std::env::args().nth(1).as_deref() == Some("--purge-credentials") {
        std::process::exit(cred::purge().is_err() as i32);
    }
    // 带 .eml 参数时直接显示这封，隐私测试用（scripts/drive.ps1）
    let eml = std::env::args().nth(1).map(|p| message::parse(&std::fs::read(&p).expect("读取邮件")).expect("解析邮件"));
    let path = store::path().expect("数据目录");
    let db = Store::open(&path).expect("数据库");
    // 默认的 Assets 只带组件自用的百来个图标，文件夹、附件等图标要全套 Lucide
    gpui_platform::application().with_assets(gpui_kit_assets::AllAssets).run(move |cx: &mut App| {
        gpui_component::init(cx);
        cx.set_global(i18n::Lang(i18n::initial(db.setting("language").ok().flatten().as_deref())));
        let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(800.), px(480.))),
            titlebar: Some(TitlebarOptions { title: Some("MailPro".into()), ..Default::default() }),
            // 不透明模式会先把整幅画面清成白色，盖住 WebView2
            window_background: WindowBackgroundAppearance::Transparent,
            ..Default::default()
        };
        cx.open_window(options, |window, cx| {
            let view = cx.new(|cx| view::MailPro::new(db, path, eml, window, cx));
            // 根视图默认铺满主题背景色，同样会盖住 WebView2；改成透明，各栏自己画背景
            cx.new(|cx| Root::new(view, window, cx).bg(transparent_black()))
        })
        .expect("open window");
    });
}
