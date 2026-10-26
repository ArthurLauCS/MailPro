//! 锁死的 WebView2 正文查看器，作为子窗口挂在 gpui 窗口上。
//!
//! 不跑脚本；所有请求都进 `WebResourceRequested`，只有 [`BASE`] 下的正文和内嵌图片有应答，
//! 远程图片（`img/n`）挂起后交给界面决定是否代取，其余在本地回 403，内核自己从不出网；
//! 所有页面跳转都取消，点链接只把编号报给界面。
//! 整个程序一个实例（系统换深浅色时重建），换邮件时改 [`Current`] 再 [`show`]。
//! 创建全程走异步回调（由 gpui 的消息循环派发），不嵌套消息循环，避免重入 gpui。

use futures::channel::mpsc::UnboundedSender;
use mailpro_core::{message::Message, sanitize::BASE};
use std::cell::RefCell;
use std::rc::Rc;
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    CoreWebView2EnvironmentOptions, CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    NavigationStartingEventHandler, NewWindowRequestedEventHandler, WebResourceRequestedEventHandler, take_pwstr,
};
use windows::Win32::Foundation::{E_POINTER, HWND};
use windows::Win32::UI::Shell::SHCreateMemStream;
use windows::core::{Error, HSTRING, Interface, PCWSTR, PWSTR, Result, w};

pub enum Event {
    Ready(ICoreWebView2Controller),
    /// 正文试图加载、被本地挡下的地址
    Blocked(String),
    /// 点了正文里第 n 个链接
    Link(usize),
    /// 正文要第 n 张远程图片，用 [`Pending::reply`] 应答
    Image(usize, Pending),
}

/// 挂起的远程图片请求；不论是否 [`reply`](Pending::reply)，丢掉时都会应答（没给图片就回 404），
/// 不应答的请求会真的发出去
pub struct Pending {
    env: ICoreWebView2Environment,
    args: ICoreWebView2WebResourceRequestedEventArgs,
    deferral: ICoreWebView2Deferral,
    img: Option<(String, Vec<u8>)>,
}

impl Pending {
    /// (Content-Type, 内容)；None 回 404
    pub fn reply(mut self, img: Option<(String, Vec<u8>)>) {
        self.img = img;
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        let (status, ct, body) = match &self.img {
            Some((ct, body)) => (200, ct.as_str(), body.as_slice()),
            None => (404, "text/plain", &[][..]),
        };
        unsafe {
            let _ = respond(&self.env, &self.args, status, ct, body).inspect_err(|e| eprintln!("WebView2: {e}"));
            let _ = self.deferral.Complete();
        }
    }
}

/// 第二道防线：即使清洗漏了，页面也只能从本地地址取图片、只能用内联样式
const CSP: &str = "default-src 'none'; img-src https://mailpro.localhost data:; style-src 'unsafe-inline'; font-src data:";

const BROWSER_ARGS: &str = "--disable-background-networking --disable-component-update --disable-domain-reliability";

/// 正在显示的邮件；请求正文和内嵌图片时从这里取
pub type Current = Rc<RefCell<Option<Message>>>;

/// 异步创建查看器，建好后发出 [`Event::Ready`]。
/// `dark`：用 Chromium 的自动深色把正文变深，`bg` 为页面没写底色时的底色。
/// 这个开关只能在创建时给，换深浅色要重建；两种参数的内核不能共用一个数据目录，所以分开放
pub fn create(hwnd: HWND, msg: Current, tx: UnboundedSender<Event>, dark: Option<COREWEBVIEW2_COLOR>) -> Result<()> {
    let data = std::env::var("LOCALAPPDATA").unwrap_or_default() + if dark.is_some() { "\\MailPro\\WebView2Dark" } else { "\\MailPro\\WebView2" };
    let options = CoreWebView2EnvironmentOptions::default();
    // 关掉内核自己的后台联网：组件更新、域名可靠性上报等
    let args = if dark.is_some() { format!("{BROWSER_ARGS} --enable-features=WebContentsForceDark") } else { BROWSER_ARGS.into() };
    unsafe { options.set_additional_browser_arguments(args) };
    let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |hr, env| {
        hr?;
        let env = env.ok_or(Error::from(E_POINTER))?;
        unsafe { create_controller(hwnd, env, msg, tx, dark) }.inspect_err(|e| eprintln!("WebView2: {e}"))
    }));
    unsafe {
        CreateCoreWebView2EnvironmentWithOptions(
            PCWSTR::null(),
            &HSTRING::from(data),
            &ICoreWebView2EnvironmentOptions::from(options),
            &handler,
        )
    }
}

unsafe fn create_controller(hwnd: HWND, env: ICoreWebView2Environment, msg: Current, tx: UnboundedSender<Event>, dark: Option<COREWEBVIEW2_COLOR>) -> Result<()> {
    unsafe {
        let env10: ICoreWebView2Environment10 = env.cast()?;
        let opts = env10.CreateCoreWebView2ControllerOptions()?;
        // 无痕：不落 cookie、缓存
        opts.SetIsInPrivateModeEnabled(true)?;
        let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |hr, c| {
            hr?;
            let c = c.ok_or(Error::from(E_POINTER))?;
            if let Some(bg) = dark {
                c.cast::<ICoreWebView2Controller2>()?.SetDefaultBackgroundColor(bg)?;
            }
            setup(&env, &c, msg, tx.clone()).inspect_err(|e| eprintln!("WebView2: {e}"))?;
            let _ = tx.unbounded_send(Event::Ready(c));
            Ok(())
        }));
        env10.CreateCoreWebView2ControllerWithOptions(hwnd, &opts, &handler)
    }
}

/// 显示 [`Current`] 里的邮件；`seq` 每次不同，保证重新加载
pub fn show(c: &ICoreWebView2Controller, seq: u32) -> Result<()> {
    unsafe { c.CoreWebView2()?.Navigate(&HSTRING::from(format!("{BASE}msg/{seq}"))) }
}

unsafe fn respond(env: &ICoreWebView2Environment, args: &ICoreWebView2WebResourceRequestedEventArgs, status: i32, ct: &str, body: &[u8]) -> Result<()> {
    let headers = format!("Content-Type: {ct}\nContent-Security-Policy: {CSP}\nCache-Control: no-store");
    unsafe {
        let stream = if body.is_empty() { None } else { SHCreateMemStream(Some(body)) };
        // 说明文字不能为空：空的会让这次加载一直挂起
        let resp = env.CreateWebResourceResponse(stream.as_ref(), status, w!("OK"), &HSTRING::from(headers))?;
        args.SetResponse(&resp)
    }
}

unsafe fn setup(env: &ICoreWebView2Environment, c: &ICoreWebView2Controller, msg: Current, tx: UnboundedSender<Event>) -> Result<()> {
    unsafe {
        let wv = c.CoreWebView2()?;
        let s = wv.Settings()?;
        s.SetIsScriptEnabled(false)?;
        s.SetIsWebMessageEnabled(false)?;
        s.SetAreHostObjectsAllowed(false)?;
        // 状态栏会显示改写后的本地链接地址，没意义
        s.SetIsStatusBarEnabled(false)?;
        s.SetAreDevToolsEnabled(cfg!(debug_assertions))?;
        let s4: ICoreWebView2Settings4 = s.cast()?;
        s4.SetIsGeneralAutofillEnabled(false)?;
        s4.SetIsPasswordAutosaveEnabled(false)?;
        // SmartScreen 会把访问的网址发给微软
        s.cast::<ICoreWebView2Settings8>()?.SetIsReputationCheckingRequired(false)?;

        let mut token = 0;
        let env = env.clone();
        let blocked = tx.clone();
        wv.cast::<ICoreWebView2_22>()?.AddWebResourceRequestedFilterWithRequestSourceKinds(
            w!("*"),
            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
            COREWEBVIEW2_WEB_RESOURCE_REQUEST_SOURCE_KINDS_ALL,
        )?;
        wv.add_WebResourceRequested(
            &WebResourceRequestedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let mut uri = PWSTR::null();
                args.Request()?.Uri(&mut uri)?;
                let uri = take_pwstr(uri);
                if let Some(n) = uri.strip_prefix(BASE).and_then(|r| r.strip_prefix("img/")).and_then(|n| n.parse().ok()) {
                    let p = Pending { env: env.clone(), args: args.clone(), deferral: args.GetDeferral()?, img: None };
                    let _ = blocked.unbounded_send(Event::Image(n, p));
                    return Ok(());
                }
                // 不管有没有邮件都要应答，不应答的请求会真的发出去
                let cur = msg.borrow();
                let html = cur.as_ref().map_or("", |m| m.body.html.as_str());
                let inline = |n: &str| cur.as_ref()?.inline.get(n.parse::<usize>().ok()?)?.as_ref();
                let (status, ct, body): (i32, &str, &[u8]) = match uri.strip_prefix(BASE) {
                    Some(rest) if rest.starts_with("msg/") => (200, "text/html; charset=utf-8", html.as_bytes()),
                    Some(rest) => match rest.strip_prefix("cid/").and_then(inline) {
                        Some((ct, data)) => (200, ct, data),
                        None => (404, "text/plain", b""),
                    },
                    None => {
                        let _ = blocked.unbounded_send(Event::Blocked(uri));
                        (403, "text/plain", b"")
                    }
                };
                respond(&env, &args, status, ct, body)
            })),
            &mut token,
        )?;

        let link = |uri: &str| uri.strip_prefix(BASE)?.strip_prefix("link/")?.parse().ok();
        let nav = tx.clone();
        wv.add_NavigationStarting(
            &NavigationStartingEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let mut uri = PWSTR::null();
                args.Uri(&mut uri)?;
                let uri = take_pwstr(uri);
                if !uri.starts_with(&format!("{BASE}msg/")) {
                    args.SetCancel(true)?;
                    if let Some(n) = link(&uri) {
                        let _ = nav.unbounded_send(Event::Link(n));
                    }
                }
                Ok(())
            })),
            &mut token,
        )?;
        // Ctrl+点击、中键、右键“在新窗口打开”
        wv.add_NewWindowRequested(
            &NewWindowRequestedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                args.SetHandled(true)?;
                let mut uri = PWSTR::null();
                args.Uri(&mut uri)?;
                if let Some(n) = link(&take_pwstr(uri)) {
                    let _ = tx.unbounded_send(Event::Link(n));
                }
                Ok(())
            })),
            &mut token,
        )
    }
}
