//! 主窗口三栏：账号和文件夹 / 邮件列表 / 阅读区，栏宽可拖。
//! 同步在后台引擎（[`engine`]）里跑，这里读数据库、发命令、按引擎的事件刷新。

use crate::i18n::{EN, Lang, ROLES, T, ZH, t};
use crate::theme::{self, DISPLAY, PROSE, R_MD};
use crate::{browser, webview};
use chrono::{DateTime, Datelike, Local};
use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use futures::channel::oneshot;
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Bounds, BoxShadow, ClickEvent, ClipboardItem, Context, DismissEvent, Entity, FontWeight, Modifiers, MouseButton, PathPromptOptions,
    Pixels, ScrollStrategy, SharedString, Subscription, Task, UniformListScrollHandle, WeakEntity, Window, canvas, div, hsla, point, px, relative,
    uniform_list, white,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::dialog::Dialog;
use gpui_component::input::{Input, InputState};
use gpui_component::menu::{ContextMenuExt, DropdownMenu, PopupMenuItem};
use gpui_component::notification::Notification;
use gpui_component::resizable::{h_resizable, resizable_panel};
use gpui_component::scroll::ScrollableElement;
use gpui_component::{ActiveTheme, Disableable, Icon, Sizable, WindowExt, h_flex, v_flex};
use gpui_kit_assets::IconName;
use mailpro_core::cred;
use mailpro_core::engine::{self, BodyError, Cmd, Event};
use mailpro_core::imap::{self, folder_name};
use mailpro_core::link::{self, Link};
use mailpro_core::message::Message;
use mailpro_core::store::{self, Account, Folder, Header, Store};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::cell::Cell;
use std::collections::HashSet;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;
use webview2_com::Microsoft::Web::WebView2::Win32::{COREWEBVIEW2_COLOR, ICoreWebView2Controller};
use windows::Win32::Foundation::{HWND, RECT};

/// 邮件列表一行的高度；虚拟列表要求每行等高
const ROW: f32 = 56.;
/// 收件箱这么多秒内收到过邮件的账号行泛光
const GLOW: i64 = 30 * 60;

struct Acct {
    acc: Account,
    /// 文件夹及未读数，按 [`Store::folders`] 的顺序
    folders: Vec<(Folder, u32)>,
    /// 收件箱最新一封的收到时间，没有为 0
    latest: i64,
    /// 连接断开的原因，重新连上后清掉
    error: Option<String>,
}

/// 拖动中的账号，跟着鼠标画一个强调色的小条
#[derive(Clone)]
struct DraggedAccount {
    id: i64,
    email: SharedString,
}

impl Render for DraggedAccount {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div().px_3().py_1().rounded(R_MD).bg(theme.primary).text_color(theme.primary_foreground).text_sm().font_weight(FontWeight::SEMIBOLD).child(self.email.clone())
    }
}

/// 对选中邮件的操作，阅读区操作栏和右键菜单共用；bool 为设上还是去掉
#[derive(Clone, Copy)]
enum Act {
    Archive,
    Delete,
    Move,
    Seen(bool),
    Star(bool),
}

enum Body {
    Loading,
    /// 正文在 [`webview::Current`] 里
    Shown,
    Failed(String),
}

struct Reading {
    /// 0 为命令行打开的 .eml，不属于任何账号，不加载远程图片
    account: i64,
    folder: i64,
    uid: u32,
    /// 0 为不知道（命令行打开的 .eml）
    date: i64,
    subject: String,
    from: String,
    /// 发件人地址，按它查“总是加载图片”名单
    addr: String,
    body: Body,
    /// 每张远程图片的域名，正文到了才有
    hosts: Vec<String>,
    /// 查看器在本地挡下的请求的域名
    blocked: Vec<String>,
    /// 这次查看加载远程图片（按封允许只管这一次）
    images: bool,
    /// 发件人在“总是加载图片”名单里
    trusted: bool,
}

impl Reading {
    fn new(account: i64, folder: i64, uid: u32, date: i64, subject: String, from: String, addr: String, trusted: bool) -> Self {
        let (body, hosts, blocked) = (Body::Loading, Vec::new(), Vec::new());
        Reading { account, folder, uid, date, subject, from, addr, body, hosts, blocked, images: trusted, trusted }
    }

    fn shown(&mut self, m: &Message) {
        (self.subject, self.from, self.body) = (m.subject.clone(), m.from.clone(), Body::Shown);
        self.hosts = m.body.images.iter().filter_map(|u| link::host(u)).collect();
    }
}

/// 点链接后的确认对话框
struct LinkDlg {
    raw: String,
    link: Link,
    /// 正在等 AdsPower 打开环境
    busy: bool,
    error: Option<String>,
}

/// AdsPower 环境设置对话框
struct AdsDlg {
    id: Entity<InputState>,
    key: Entity<InputState>,
    /// 凭据管理器里已有 key
    saved: bool,
    error: Option<String>,
}

/// 添加账号 / 账号设置对话框的内容
struct Form {
    /// 账号设置时为该账号，只改代理
    account: Option<Account>,
    email: Entity<InputState>,
    secret: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    proxy: Entity<InputState>,
    busy: bool,
    error: Option<String>,
}

pub struct MailPro {
    db: Store,
    path: String,
    cmd: UnboundedSender<Cmd>,
    accounts: Vec<Acct>,
    /// 选中的（账号 id，文件夹 id）
    sel: Option<(i64, i64)>,
    /// 选中文件夹的全部邮件头，新的在前
    headers: Vec<Header>,
    /// 列表里选中的邮件（uid）；只选一封且打开了时阅读区显示它，否则显示选了几封和操作栏
    picked: HashSet<u32>,
    /// Shift 点选的起点：上次直接点或 Ctrl 点的那封
    anchor: Option<u32>,
    /// 右键菜单开着时有值。这时正文藏起来：盖在正文上的菜单项点不到，点击会落进 WebView2
    menu: Option<Subscription>,
    scroll: UniformListScrollHandle,
    load: Task<()>,
    reading: Option<Reading>,
    current: webview::Current,
    web: Option<ICoreWebView2Controller>,
    /// 收当前查看器事件的任务；换掉就不再收旧查看器的
    web_events: Task<()>,
    /// 上次给 WebView2 的位置（物理像素）和可见性，没变就不再设
    placed: Rc<Cell<Option<(RECT, bool)>>>,
    /// 每次显示新邮件加一，让 WebView2 重新加载
    seq: u32,
    form: Option<Entity<Form>>,
    /// 打开链接用的浏览器，格式见 [`browser`]
    browser: String,
    /// 左栏收起的账号 id，存在设置 `collapsed` 里（空格分隔），重启后保持
    collapsed: HashSet<i64>,
    /// 等到最早一个账号该熄灭泛光时重画
    glow: Task<()>,
}

impl MailPro {
    /// `eml`：命令行带了 .eml 时直接显示这封，隐私测试用
    pub fn new(db: Store, path: String, eml: Option<Message>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        theme::apply(theme::system_dark(window), window, cx);
        // 深浅色跟随系统；正文的深色只能在建查看器时给，换了就重建
        cx.observe_window_appearance(window, |v, window, cx| {
            let dark = theme::system_dark(window);
            if dark != cx.theme().is_dark() {
                theme::apply(dark, window, cx);
                v.viewer(window, cx);
            }
        })
        .detach();

        let (cmd, mut events) = engine::spawn(path.clone());
        cx.spawn_in(window, async move |this, cx| {
            while let Some(e) = events.next().await {
                if this.update_in(cx, |v, window, cx| v.on_engine(e, window, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        let current = webview::Current::default();
        let reading = eml.map(|m| {
            let mut r = Reading::new(0, 0, 0, 0, String::new(), String::new(), String::new(), false);
            r.shown(&m);
            *current.borrow_mut() = Some(m);
            r
        });
        let browser = db.setting("browser").ok().flatten().unwrap_or_default();
        let collapsed = db.setting("collapsed").ok().flatten().unwrap_or_default().split_whitespace().filter_map(|s| s.parse().ok()).collect();
        let mut v = MailPro {
            db,
            path,
            cmd,
            accounts: Vec::new(),
            sel: None,
            headers: Vec::new(),
            picked: HashSet::new(),
            anchor: None,
            menu: None,
            scroll: UniformListScrollHandle::new(),
            load: Task::ready(()),
            reading,
            current,
            web: None,
            web_events: Task::ready(()),
            placed: Rc::default(),
            seq: 0,
            form: None,
            browser,
            collapsed,
            glow: Task::ready(()),
        };
        v.viewer(window, cx);
        v.reload_accounts();
        v.select_first(None, cx);
        v.glow(cx);
        v
    }

    /// 建正文查看器，深浅色跟当前主题；已有的先关掉
    fn viewer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(c) = self.web.take() {
            let _ = unsafe { c.Close() };
        }
        self.placed.set(None);
        let RawWindowHandle::Win32(h) = window.window_handle().expect("window handle").as_raw() else { unreachable!() };
        let bg = cx.theme().is_dark().then(|| {
            let c = cx.theme().background.to_rgb();
            let u = |v: f32| (v * 255.).round() as u8;
            COREWEBVIEW2_COLOR { A: 255, R: u(c.r), G: u(c.g), B: u(c.b) }
        });
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        webview::create(HWND(h.hwnd.get() as _), self.current.clone(), tx, bg).expect("WebView2");
        self.web_events = cx.spawn_in(window, async move |this, cx| {
            while let Some(e) = rx.next().await {
                if this.update_in(cx, |v, window, cx| v.on_webview(e, window, cx)).is_err() {
                    break;
                }
            }
        });
    }

    fn reload_accounts(&mut self) {
        let old = std::mem::take(&mut self.accounts);
        // 拖动排过的顺序存在设置 `order` 里（空格分隔的 id），没排过的按添加顺序接在后面
        let order: Vec<i64> = self.db.setting("order").ok().flatten().unwrap_or_default().split_whitespace().filter_map(|s| s.parse().ok()).collect();
        let mut accounts = self.db.accounts().unwrap_or_default();
        accounts.sort_by_key(|a| order.iter().position(|&id| id == a.id).unwrap_or(usize::MAX));
        for acc in accounts {
            let error = old.iter().find(|a| a.acc.id == acc.id).and_then(|a| a.error.clone());
            let id = acc.id;
            self.accounts.push(Acct { acc, folders: Vec::new(), latest: 0, error });
            self.reload_folders(id);
        }
    }

    fn reload_folders(&mut self, account: i64) {
        let db = &self.db;
        let Some(a) = self.accounts.iter_mut().find(|a| a.acc.id == account) else { return };
        let folders = db.folders(account).unwrap_or_default();
        a.folders = folders.into_iter().map(|f| (db.stats(f.id).map_or(0, |s| s.1), f)).map(|(n, f)| (f, n)).collect();
        a.latest = a.folders.iter().find(|f| f.0.role.as_deref() == Some("inbox")).map_or(0, |f| db.latest(f.0.id).unwrap_or(0));
    }

    /// 把账号 `from` 挪到 `to` 的位置：往下拖落在它后面，往上拖落在它前面
    fn move_account(&mut self, from: i64, to: i64, cx: &mut Context<Self>) {
        let pos = |id| self.accounts.iter().position(|a| a.acc.id == id);
        let (Some(i), Some(j)) = (pos(from), pos(to)) else { return };
        let a = self.accounts.remove(i);
        self.accounts.insert(j, a);
        let ids: Vec<_> = self.accounts.iter().map(|a| a.acc.id.to_string()).collect();
        let _ = self.db.set_setting("order", &ids.join(" "));
        cx.notify();
    }

    /// 泛光到期不会有引擎事件来触发重画：等到最早一个账号该熄灭的时刻重画一次，再排下一个
    fn glow(&mut self, cx: &mut Context<Self>) {
        let now = Local::now().timestamp();
        let next = self.accounts.iter().map(|a| a.latest + GLOW - now).filter(|&s| s > 0).min();
        self.glow = match next {
            Some(s) => cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_secs(s as u64)).await;
                let _ = this.update(cx, |v, cx| {
                    v.glow(cx);
                    cx.notify();
                });
            }),
            None => Task::ready(()),
        };
    }

    /// 还没选文件夹时选第一个（收件箱排在最前）；`account` 给定时只在该账号里选
    fn select_first(&mut self, account: Option<i64>, cx: &mut Context<Self>) {
        if self.sel.is_some() {
            return;
        }
        let first = self.accounts.iter().filter(|a| account.is_none_or(|id| id == a.acc.id)).find_map(|a| Some((a.acc.id, a.folders.first()?.0.id)));
        if let Some((a, f)) = first {
            self.select(a, f, cx);
        }
    }

    fn select(&mut self, account: i64, folder: i64, cx: &mut Context<Self>) {
        if self.sel != Some((account, folder)) {
            self.sel = Some((account, folder));
            self.headers.clear();
            (self.picked, self.anchor) = (HashSet::new(), None);
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        }
        self.load(cx);
    }

    /// 在后台线程另开连接读选中文件夹的邮件头，大文件夹不卡界面
    fn load(&mut self, cx: &mut Context<Self>) {
        let Some((_, folder)) = self.sel else { return };
        let path = self.path.clone();
        self.load = cx.spawn(async move |this, cx| {
            let headers = cx.background_spawn(async move { Store::open(&path).and_then(|db| db.list(folder, u32::MAX)) }).await;
            let _ = this.update(cx, |v, cx| {
                if v.sel.is_some_and(|s| s.1 == folder) {
                    v.headers = headers.unwrap_or_default();
                    cx.notify();
                }
            });
        });
    }

    fn folder(&self, account: i64, folder: i64) -> Option<Folder> {
        let a = self.accounts.iter().find(|a| a.acc.id == account)?;
        Some(a.folders.iter().find(|f| f.0.id == folder)?.0.clone())
    }

    /// 打开一封：标已读，再取正文
    fn open(&mut self, uid: u32, cx: &mut Context<Self>) {
        let Some((account, folder)) = self.sel else { return };
        let Some(f) = self.folder(account, folder) else { return };
        self.mark(store::SEEN, true, |h| h.uid == uid, cx);
        let Some(h) = self.headers.iter().find(|h| h.uid == uid) else { return };
        let from = if h.from_name.is_empty() { h.from_addr.clone() } else { h.from_name.clone() };
        let trusted = self.db.images_allowed(&h.from_addr).unwrap_or(false);
        self.reading = Some(Reading::new(account, folder, uid, h.date, h.subject.clone(), from, h.from_addr.clone(), trusted));
        let _ = self.cmd.unbounded_send(Cmd::Body { account, folder: f, uid });
        cx.notify();
    }

    /// 点一行：直接点选中并打开这封；Ctrl 点增减一封、Shift 点从上次点的那封选到这封，都不打开
    fn click(&mut self, uid: u32, m: Modifiers, cx: &mut Context<Self>) {
        let index = |uid| self.headers.iter().position(|h| h.uid == uid);
        if m.shift
            && let Some((a, b)) = self.anchor.and_then(index).zip(index(uid))
        {
            self.picked = self.headers[a.min(b)..=a.max(b)].iter().map(|h| h.uid).collect();
        } else if m.secondary() || m.shift {
            if !self.picked.remove(&uid) {
                self.picked.insert(uid);
            }
            self.anchor = Some(uid);
        } else {
            (self.picked, self.anchor) = (HashSet::from([uid]), Some(uid));
            return self.open(uid, cx);
        }
        self.reading = None;
        cx.notify();
    }

    /// 给 `pick` 选出的邮件设上 / 去掉标记：先改本地，再让引擎写回服务器
    fn mark(&mut self, flag: u8, on: bool, pick: impl Fn(&Header) -> bool, cx: &mut Context<Self>) {
        let Some((account, folder)) = self.sel else { return };
        let Some(f) = self.folder(account, folder) else { return };
        let hit = |h: &Header| (h.flags & flag != 0) != on && pick(h);
        let uids: Vec<u32> = self.headers.iter().filter(|h| hit(h)).map(|h| h.uid).collect();
        if uids.is_empty() || self.db.mark(folder, &uids, flag, on).is_err() {
            return;
        }
        // 选出的这些标记位都和目标相反，翻过来即是
        self.headers.iter_mut().filter(|h| hit(h)).for_each(|h| h.flags ^= flag);
        let _ = self.cmd.unbounded_send(Cmd::Flush { account, folder: f });
        self.reload_folders(account);
        cx.notify();
    }

    /// 账号里 `role` 用途的文件夹
    fn role_folder(&self, account: i64, role: &str) -> Option<Folder> {
        let a = self.accounts.iter().find(|a| a.acc.id == account)?;
        Some(a.folders.iter().find(|f| f.0.role.as_deref() == Some(role))?.0.clone())
    }

    /// 归档的去处：归档文件夹，没有就是所有邮件（Gmail 的归档即去掉当前标签）；正在看的就是它时没有
    fn archive_to(&self) -> Option<Folder> {
        let (account, folder) = self.sel?;
        self.role_folder(account, "archive").or_else(|| self.role_folder(account, "all")).filter(|f| f.id != folder)
    }

    /// 选中邮件能做的操作及其图标、文字
    fn acts(&self, t: &'static T) -> Vec<(Act, IconName, &'static str)> {
        // 选中的多在列表前部，找齐就停
        let flags: Vec<u8> = self.headers.iter().filter(|h| self.picked.contains(&h.uid)).take(self.picked.len()).map(|h| h.flags).collect();
        let unread = flags.iter().any(|f| f & store::SEEN == 0);
        let unstarred = flags.iter().any(|f| f & store::FLAGGED == 0);
        let archive = self.archive_to().map(|_| (Act::Archive, IconName::Archive, t.archive));
        archive
            .into_iter()
            .chain([
                (Act::Delete, IconName::Trash, t.delete),
                (Act::Move, IconName::FolderInput, t.move_to),
                if unread { (Act::Seen(true), IconName::MailOpen, t.mark_read) } else { (Act::Seen(false), IconName::Mail, t.mark_unread) },
                if unstarred { (Act::Star(true), IconName::Star, t.star) } else { (Act::Star(false), IconName::StarOff, t.unstar) },
            ])
            .collect()
    }

    fn act(&mut self, act: Act, window: &mut Window, cx: &mut Context<Self>) {
        let Some((account, folder)) = self.sel else { return };
        let view = cx.entity().downgrade();
        match act {
            Act::Archive => {
                if let Some(to) = self.archive_to() {
                    self.move_picked(Some(to), window, cx);
                }
            }
            // 移到已删除；已经在已删除里，或账号没有已删除文件夹，就彻底删除，先确认
            Act::Delete => match self.role_folder(account, "trash").filter(|f| f.id != folder) {
                Some(trash) => self.move_picked(Some(trash), window, cx),
                None => {
                    let n = self.picked.len();
                    window.open_dialog(cx, move |dialog, _, cx| delete_dialog(n, &view, dialog, cx));
                }
            },
            Act::Move => {
                let Some(a) = self.accounts.iter().find(|a| a.acc.id == account) else { return };
                let folders: Vec<Folder> = a.folders.iter().map(|f| f.0.clone()).filter(|f| f.id != folder).collect();
                window.open_dialog(cx, move |dialog, _, cx| move_dialog(&folders, &view, dialog, cx));
            }
            Act::Seen(on) | Act::Star(on) => {
                let flag = if matches!(act, Act::Seen(_)) { store::SEEN } else { store::FLAGGED };
                let picked = std::mem::take(&mut self.picked);
                self.mark(flag, on, |h| picked.contains(&h.uid), cx);
                self.picked = picked;
            }
        }
    }

    /// 把选中的邮件移到 `to`，None 为彻底删除：先从本地删掉，再让引擎在服务器上移。账号没连上时不动
    fn move_picked(&mut self, to: Option<Folder>, window: &mut Window, cx: &mut Context<Self>) {
        let Some((account, folder)) = self.sel else { return };
        let Some(f) = self.folder(account, folder) else { return };
        if self.accounts.iter().any(|a| a.acc.id == account && a.error.is_some()) {
            return window.push_notification(Notification::error(t(cx).offline), cx);
        }
        let uids: Vec<u32> = self.headers.iter().filter(|h| self.picked.contains(&h.uid)).map(|h| h.uid).collect();
        if uids.is_empty() || self.db.remove(folder, &uids).is_err() {
            return;
        }
        self.headers.retain(|h| !self.picked.contains(&h.uid));
        if self.reading.as_ref().is_some_and(|r| r.folder == folder && self.picked.contains(&r.uid)) {
            self.reading = None;
        }
        (self.picked, self.anchor) = (HashSet::new(), None);
        let _ = self.cmd.unbounded_send(Cmd::Move { account, folder: f, uids, to });
        self.reload_folders(account);
        cx.notify();
    }

    /// 阅读区的操作栏
    fn actions(&self, cx: &mut Context<Self>) -> AnyElement {
        let buttons = self.acts(t(cx)).into_iter().map(|(act, icon, tip)| {
            Button::new(tip).ghost().small().icon(icon).tooltip(tip).on_click(cx.listener(move |v, _, window, cx| v.act(act, window, cx)))
        });
        h_flex().flex_none().gap_1().children(buttons).into_any_element()
    }

    fn on_engine(&mut self, e: Event, window: &mut Window, cx: &mut Context<Self>) {
        let t = t(cx);
        match e {
            Event::Folders(account) => {
                if let Some(a) = self.accounts.iter_mut().find(|a| a.acc.id == account) {
                    a.error = None;
                }
                self.reload_folders(account);
                self.select_first(Some(account), cx);
            }
            Event::Changed { account, folder } => {
                self.reload_folders(account);
                if self.sel == Some((account, folder)) {
                    self.load(cx);
                }
            }
            Event::Body { folder, uid, msg } => {
                let Some(r) = self.reading.as_mut().filter(|r| r.folder == folder && r.uid == uid) else { return };
                match msg {
                    Ok(m) => {
                        r.shown(&m);
                        *self.current.borrow_mut() = Some(m);
                        self.show();
                    }
                    Err(BodyError::Gone) => r.body = Body::Failed(t.gone.into()),
                    Err(BodyError::Unparsable) => r.body = Body::Failed(t.unparsable.into()),
                    Err(BodyError::Offline) => r.body = Body::Failed(t.offline.into()),
                    Err(BodyError::Net(e)) => r.body = Body::Failed(format!("{}{e}", t.load_failed)),
                }
            }
            Event::Error { account, msg } => match self.accounts.iter_mut().find(|a| a.acc.id == account) {
                Some(a) => a.error = Some(msg),
                None => eprintln!("{msg}"),
            },
            Event::Added(Ok(account)) => {
                self.reload_accounts();
                if self.form.take().is_some() && window.has_active_dialog(cx) {
                    window.close_dialog(cx);
                }
                self.select_first(Some(account), cx);
            }
            Event::Failed(e) => window.push_notification(Notification::error(format!("{}{e}", t.action_failed)), cx),
            Event::Added(Err(e)) => {
                if let Some(f) = &self.form {
                    f.update(cx, |f, cx| {
                        (f.busy, f.error) = (false, Some(e));
                        cx.notify();
                    });
                }
            }
        }
        // 新邮件到了可能要亮起泛光，改排熄灭的时刻
        self.glow(cx);
        cx.notify();
    }

    fn on_webview(&mut self, e: webview::Event, window: &mut Window, cx: &mut Context<Self>) {
        match e {
            webview::Event::Ready(c) => {
                self.web = Some(c);
                if matches!(self.reading, Some(Reading { body: Body::Shown, .. })) {
                    self.show();
                }
            }
            webview::Event::Blocked(uri) => {
                eprintln!("blocked {uri}");
                if let Some(r) = &mut self.reading {
                    r.blocked.push(link::host(&uri).unwrap_or(uri));
                }
            }
            webview::Event::Link(n) => {
                let raw = self.current.borrow().as_ref().and_then(|m| m.body.links.get(n).cloned());
                if let Some(raw) = raw {
                    let dlg = cx.new(|_| LinkDlg { link: link::clean(&raw), raw, busy: false, error: None });
                    let view = cx.entity().downgrade();
                    window.open_dialog(cx, move |dialog, _, cx| link_dialog(&dlg, &view, dialog, cx));
                }
            }
            webview::Event::Image(n, p) => self.image(n, p, cx),
        }
        cx.notify();
    }

    /// 让 WebView2 重新加载当前邮件
    fn show(&mut self) {
        self.seq += 1;
        if let Some(c) = &self.web {
            let _ = webview::show(c, self.seq);
        }
    }

    /// 远程图片：这次查看允许加载时经该账号的代理代取，否则回 404
    fn image(&mut self, n: usize, p: webview::Pending, cx: &mut Context<Self>) {
        let url = self.current.borrow().as_ref().and_then(|m| m.body.images.get(n).cloned());
        let acc = self.reading.as_ref().filter(|r| r.images).and_then(|r| self.accounts.iter().find(|a| a.acc.id == r.account));
        let (Some(url), Some(a)) = (url, acc) else { return p.reply(None) };
        let (reply, rx) = oneshot::channel();
        let _ = self.cmd.unbounded_send(Cmd::Image { url, proxy: a.acc.proxy.clone(), reply });
        cx.spawn(async move |_, _| p.reply(rx.await.ok().flatten())).detach();
    }

    /// 改这次查看是否加载远程图片；`trusted` 给定时同时改发件人名单
    fn set_images(&mut self, images: bool, trusted: Option<bool>, cx: &mut Context<Self>) {
        let Some(r) = &mut self.reading else { return };
        if let Some(trusted) = trusted {
            let _ = self.db.allow_images(&r.addr, trusted);
            r.trusted = trusted;
        }
        r.images = images;
        self.show();
        cx.notify();
    }

    fn toggle(&mut self, account: i64, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&account) {
            self.collapsed.insert(account);
        }
        let ids: Vec<_> = self.collapsed.iter().map(i64::to_string).collect();
        let _ = self.db.set_setting("collapsed", &ids.join(" "));
        cx.notify();
    }

    fn set_browser(&mut self, setting: String, cx: &mut Context<Self>) {
        let _ = self.db.set_setting("browser", &setting);
        self.browser = setting;
        cx.notify();
    }

    /// AdsPower 环境的设置：环境 ID 和 API key（key 进凭据管理器，留空不改）
    fn open_adspower(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let t = t(cx);
        let id = browser::adspower(&self.browser).unwrap_or_default().to_string();
        let saved = cred::load(browser::KEY).is_ok();
        let id = cx.new(|cx| InputState::new(window, cx).default_value(id));
        let key = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder(if saved { t.key_saved } else { "" }));
        let dlg = cx.new(|_| AdsDlg { id, key, saved, error: None });
        let view = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| adspower_dialog(&dlg, &view, dialog, cx));
    }

    fn choose_browser(&mut self, cx: &mut Context<Self>) {
        let prompt = Some(t(cx).choose_browser.into());
        let paths = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await
                && let Some(exe) = paths.first()
            {
                let _ = this.update(cx, |v, cx| v.set_browser(exe.to_string_lossy().into_owned(), cx));
            }
        })
        .detach();
    }

    fn set_language(&mut self, t: &'static T, cx: &mut Context<Self>) {
        cx.set_global(Lang(t));
        let _ = self.db.set_setting("language", t.code);
        cx.refresh_windows();
    }

    /// `account` 为 None 是添加账号，否则是该账号的设置（只改代理）
    fn open_form(&mut self, account: Option<Account>, window: &mut Window, cx: &mut Context<Self>) {
        let mut input = |placeholder: &str, value: &str, masked: bool| {
            let (placeholder, value) = (placeholder.to_string(), value.to_string());
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(value).masked(masked))
        };
        let proxy = account.as_ref().and_then(|a| a.proxy.clone()).unwrap_or_default();
        let (email, secret) = (input("name@gmail.com", "", false), input("", "", true));
        let (host, port, proxy) = (input("imap.example.com", "", false), input("", "993", false), input("127.0.0.1:7898", &proxy, false));
        let form = cx.new(|_| Form { account, email, secret, host, port, proxy, busy: false, error: None });
        self.form = Some(form.clone());
        let view = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| form_dialog(&form, &view, dialog, cx));
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.form.clone() else { return };
        let t = t(cx);
        let f = form.read(cx);
        let text = |e: &Entity<InputState>| e.read(cx).value().trim().to_string();
        let (email, host, port, proxy) = (text(&f.email), text(&f.host), text(&f.port), text(&f.proxy));
        let secret = f.secret.read(cx).value().to_string();
        let proxy = Some(proxy).filter(|p| !p.is_empty());
        if let Some(acc) = f.account.clone() {
            // 账号设置只改代理，按新设置重连
            let _ = self.db.set_proxy(acc.id, proxy.as_deref());
            let _ = self.cmd.unbounded_send(Cmd::Restart(acc.id));
            self.reload_accounts();
            self.form = None;
            window.close_dialog(cx);
            return;
        }
        let host = imap::default_host(&email).map(str::to_string).or(Some(host).filter(|h| !h.is_empty()));
        let error = match &host {
            _ if imap::oauth_only(&email) => Some(t.oauth_only),
            None => Some(t.need_server),
            Some(_) => None,
        };
        if let (None, Some(host)) = (error, host) {
            let acc = Account { id: 0, email, host, port: port.parse().unwrap_or(993), proxy };
            let _ = self.cmd.unbounded_send(Cmd::Add(acc, secret));
        }
        form.update(cx, |f, cx| {
            (f.busy, f.error) = (error.is_none(), error.map(Into::into));
            cx.notify();
        });
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (t, theme) = (t(cx), cx.theme());
        let view = cx.entity().downgrade();
        let now = Local::now().timestamp();
        let accounts = self.accounts.iter().map(|a| {
            let id = a.acc.id;
            let acc = a.acc.clone();
            let collapsed = self.collapsed.contains(&id);
            // 收起时正在看的文件夹藏在里面，由账号行代它高亮
            let lit = collapsed && self.sel.is_some_and(|s| s.0 == id);
            let inbox = a.folders.iter().find(|f| f.0.role.as_deref() == Some("inbox")).map_or(0, |f| f.1);
            // 收件箱最近收到过邮件：账号行淡淡地透出强调色，四周一圈柔光
            let glow = now - a.latest < GLOW;
            let halo = BoxShadow { color: theme.primary.opacity(0.22), offset: point(px(0.), px(0.)), blur_radius: px(6.), spread_radius: px(0.), inset: false };
            let dragged = DraggedAccount { id, email: a.acc.email.clone().into() };
            let head = h_flex()
                .gap_1()
                .child(
                    h_flex()
                        .id(("account", id as usize))
                        .flex_1()
                        .min_w_0()
                        .gap_1p5()
                        .px_2()
                        .py_1()
                        .rounded(R_MD)
                        .cursor_pointer()
                        .when(glow, |d| d.bg(theme.primary.opacity(0.08)).shadow(vec![halo]))
                        .when(lit, |d| d.bg(theme.list_active))
                        .when(!lit, |d| d.hover(|d| d.bg(theme.list_hover)))
                        // 拖到别的账号上松开就排到它的位置
                        .on_drag(dragged, |d, _, _, cx| cx.new(|_| d.clone()))
                        .child(Icon::new(if collapsed { IconName::ChevronRight } else { IconName::ChevronDown }).xsmall().text_color(theme.muted_foreground))
                        .child(avatar(id, &a.acc.email))
                        .child(div().flex_1().truncate().text_sm().font_weight(FontWeight::SEMIBOLD).child(a.acc.email.clone()))
                        .when(a.acc.proxy.is_none(), |d| d.child(div().text_xs().text_color(theme.warning).child(t.direct)))
                        // 收起时断线只亮红点，原因展开后看
                        .when(collapsed && a.error.is_some(), |d| d.child(div().size(px(7.)).flex_none().rounded_full().bg(theme.danger)))
                        .when(collapsed && inbox > 0, |d| d.child(div().text_xs().text_color(theme.muted_foreground).child(inbox.to_string())))
                        .on_click(cx.listener(move |v, _, _, cx| v.toggle(id, cx))),
                )
                .child(
                    Button::new(("settings", id as usize))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Settings)
                        .tooltip(t.account_settings)
                        .on_click(cx.listener(move |v, _, window, cx| v.open_form(Some(acc.clone()), window, cx))),
                );
            let folders = a.folders.iter().map(|(f, unread)| {
                let fid = f.id;
                let selected = self.sel == Some((id, fid));
                h_flex()
                    .id(("folder", fid as usize))
                    .gap_2()
                    // 图标和账号的圆标对齐
                    .pl(px(30.))
                    .pr_2()
                    .py_1()
                    .rounded(R_MD)
                    .text_sm()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(theme.list_active).text_color(theme.link).font_weight(FontWeight::SEMIBOLD))
                    .when(!selected, |d| d.hover(|d| d.bg(theme.list_hover)))
                    .child(Icon::new(folder_icon(f)).small().text_color(if selected { theme.link } else { theme.muted_foreground }))
                    .child(div().flex_1().truncate().child(folder_label(t, f)))
                    .when(*unread > 0, |d| d.child(div().text_xs().text_color(theme.muted_foreground).child(unread.to_string())))
                    .on_click(cx.listener(move |v, _, _, cx| v.select(id, fid, cx)))
            });
            let target = theme.list_active;
            v_flex()
                .gap_0p5()
                .rounded(R_MD)
                .drag_over::<DraggedAccount>(move |s, _, _, _| s.bg(target))
                .on_drop(cx.listener(move |v, d: &DraggedAccount, _, cx| v.move_account(d.id, id, cx)))
                .child(head)
                .when(!collapsed, |d| {
                    d.when_some(a.error.clone(), |d, e| d.child(div().pl(px(30.)).pr_2().text_xs().text_color(theme.danger).child(format!("{}{e}", t.disconnected))))
                        .children(folders)
                })
        });
        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .child(
                v_flex()
                    .id("accounts")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_2()
                    .gap_2()
                    .when(self.accounts.is_empty(), |d| d.child(div().p_2().font_family(PROSE).text_color(theme.muted_foreground).child(t.no_accounts)))
                    .children(accounts),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .p_2()
                    .gap_1()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("add-account")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .label(t.add_account)
                            .on_click(cx.listener(|v, _, window, cx| v.open_form(None, window, cx))),
                    )
                    .child(div().flex_1())
                    .child(Button::new("browser").ghost().small().icon(IconName::Globe).tooltip(t.browser).dropdown_menu({
                        let view = view.clone();
                        // 选中的程序或 AdsPower 环境，打勾显示在“系统默认”下面
                        let current = match browser::adspower(&self.browser) {
                            Some(id) => format!("AdsPower {id}"),
                            None => std::path::Path::new(&self.browser).file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned()),
                        };
                        move |menu, _, _| {
                            let (default, choose, ads) = (view.clone(), view.clone(), view.clone());
                            menu.item(PopupMenuItem::new(t.system_browser).checked(current.is_empty()).on_click(move |_, _, cx| {
                                let _ = default.update(cx, |v, cx| v.set_browser(String::new(), cx));
                            }))
                            .when(!current.is_empty(), |m| m.item(PopupMenuItem::new(current.clone()).checked(true)))
                            .item(PopupMenuItem::new(t.choose_browser).on_click(move |_, _, cx| {
                                let _ = choose.update(cx, |v, cx| v.choose_browser(cx));
                            }))
                            .item(PopupMenuItem::new(t.adspower).on_click(move |_, window, cx| {
                                let _ = ads.update(cx, |v, cx| v.open_adspower(window, cx));
                            }))
                        }
                    }))
                    .child(Button::new("language").ghost().small().icon(IconName::Languages).tooltip(t.language).dropdown_menu(move |menu, _, _| {
                        let item = |lang: &'static T, label: &'static str| {
                            let view = view.clone();
                            PopupMenuItem::new(label).checked(t.code == lang.code).on_click(move |_, _, cx| {
                                let _ = view.update(cx, |v, cx| v.set_language(lang, cx));
                            })
                        };
                        // 选项文字固定，不随界面语言变
                        menu.item(item(&ZH, "中文/Chinese")).item(item(&EN, "English"))
                    })),
            )
    }

    fn row(&self, i: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let (t, theme) = (t(cx), cx.theme());
        let h = &self.headers[i];
        let uid = h.uid;
        let unread = h.flags & store::SEEN == 0;
        let picked = self.picked.contains(&uid);
        let from = if h.from_name.is_empty() { &h.from_addr } else { &h.from_name };
        let view = cx.entity().downgrade();
        let subject = if h.subject.is_empty() { t.no_subject.to_string() } else { h.subject.clone() };
        v_flex()
            .id(("message", uid as usize))
            .relative()
            .w_full()
            .h(px(ROW))
            .px_3()
            .gap_0p5()
            .justify_center()
            .border_b_1()
            .border_color(theme.border)
            .cursor_pointer()
            .when(picked, |d| d.bg(theme.list_active).child(div().absolute().left_0().top_0().bottom_0().w(px(3.)).bg(theme.primary)))
            .when(!picked, |d| d.hover(|d| d.bg(theme.list_hover)))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().size(px(7.)).flex_none().rounded_full().when(unread, |d| d.bg(theme.primary)))
                    .child(div().flex_1().truncate().text_sm().when(unread, |d| d.font_weight(FontWeight::SEMIBOLD)).child(from.clone()))
                    .child(div().flex_none().text_xs().text_color(theme.muted_foreground).child(short_date(t, h.date))),
            )
            .child(
                h_flex()
                    .gap_2()
                    .pl(px(15.))
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(div().flex_1().truncate().child(subject))
                    .when(h.attach, |d| d.child(Icon::new(IconName::Paperclip).xsmall()))
                    .when(h.flags & store::FLAGGED != 0, |d| d.child(Icon::new(IconName::StarFill).xsmall().text_color(theme.warning))),
            )
            .on_click(cx.listener(move |v, e: &ClickEvent, _, cx| v.click(uid, e.modifiers(), cx)))
            // 右键没选中的行先选中它；菜单下一帧才建，那时已选好
            .on_mouse_down(MouseButton::Right, cx.listener(move |v, _, _, cx| if !v.picked.contains(&uid) { v.click(uid, Modifiers::none(), cx) }))
            .context_menu(move |menu, _, cx| {
                let Some(view) = view.upgrade() else { return menu };
                // 菜单开着时正文藏起来（见 `menu` 字段），关掉时恢复
                let me = cx.weak_entity();
                let v = view.clone();
                cx.defer(move |cx| {
                    let Some(me) = me.upgrade() else { return };
                    v.update(cx, |v, cx| {
                        v.menu = Some(cx.subscribe(&me, |v, _, _: &DismissEvent, cx| {
                            v.menu = None;
                            cx.notify();
                        }));
                        cx.notify();
                    });
                });
                view.read(cx).acts(t).into_iter().fold(menu, |menu, (act, icon, label)| {
                    let view = view.downgrade();
                    menu.item(PopupMenuItem::new(label).icon(icon).on_click(move |_, window, cx| {
                        let _ = view.update(cx, |v, cx| v.act(act, window, cx));
                    }))
                })
            })
    }

    fn list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let unread = self.sel.and_then(|(a, f)| self.accounts.iter().find(|x| x.acc.id == a)?.folders.iter().find(|x| x.0.id == f).map(|x| x.1));
        let all_read = unread.is_some_and(|n| n > 0).then(|| {
            let tip = t(cx).mark_all_read;
            Button::new("all-read").ghost().xsmall().icon(IconName::CheckCheck).tooltip(tip).on_click(cx.listener(|v, _, _, cx| v.mark(store::SEEN, true, |_| true, cx)))
        });
        let (t, theme) = (t(cx), cx.theme());
        let empty = self.headers.is_empty() && self.sel.is_some();
        let rows = uniform_list(
            "messages",
            self.headers.len(),
            cx.processor(|v: &mut Self, range: Range<usize>, _, cx| range.map(|i| v.row(i, cx)).collect::<Vec<_>>()),
        )
        .track_scroll(&self.scroll)
        .size_full();
        // 顶部标题：文件夹、所属账号、封数，多个邮箱时知道在看哪个
        let head = self.sel.and_then(|(a, f)| {
            let email = self.accounts.iter().find(|x| x.acc.id == a)?.acc.email.clone();
            let muted = |s: String| div().text_xs().text_color(theme.muted_foreground).child(s);
            Some(
                h_flex()
                    .flex_none()
                    .h(px(40.))
                    .px_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().flex_none().font_family(DISPLAY).font_weight(FontWeight::BOLD).child(folder_label(t, &self.folder(a, f)?)))
                    .child(muted(email).flex_1().truncate())
                    .child(muted(format!("{}{}", self.headers.len(), t.messages)).flex_none())
                    .children(all_read),
            )
        });
        v_flex().size_full().bg(theme.background).children(head).child(
            div()
                .flex_1()
                .min_h_0()
                .relative()
                .when(empty, |d| d.flex().items_center().justify_center().font_family(PROSE).text_color(theme.muted_foreground).child(t.no_messages))
                .when(!empty, |d| d.child(rows).vertical_scrollbar(&self.scroll)),
        )
    }

    fn reader(&self, show: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let remote = self.reading.as_ref().filter(|r| matches!(r.body, Body::Shown)).map(|r| self.remote(r, cx));
        // 操作栏：打开了的那封在头部，多选时在中间
        let actions = (!self.picked.is_empty()).then(|| self.actions(cx));
        let (head_actions, picked_actions) = if self.reading.is_some() { (actions, None) } else { (None, actions) };
        let (t, theme) = (t(cx), cx.theme());
        let (c, placed) = (self.web.clone(), self.placed.clone());
        // 每帧量出正文区的位置交给 WebView2；没有正文或开着对话框时藏起来，否则点击会落到正文上
        let place = move |b: Bounds<Pixels>, window: &mut Window, _: &mut App| {
            let Some(c) = &c else { return };
            let s = window.scale_factor();
            let px = |v: Pixels| (f32::from(v) * s).round() as i32;
            let r = RECT { left: px(b.origin.x), top: px(b.origin.y), right: px(b.origin.x + b.size.width), bottom: px(b.origin.y + b.size.height) };
            if placed.replace(Some((r, show))) != Some((r, show)) {
                let _ = unsafe { c.SetBounds(r) };
                let _ = unsafe { c.SetIsVisible(show) };
            }
        };
        let status = match &self.reading {
            None if !self.picked.is_empty() => format!("{}{}{}", t.picked, self.picked.len(), t.messages),
            None => t.no_selection.to_string(),
            Some(Reading { body: Body::Loading, .. }) => t.loading.to_string(),
            Some(Reading { body: Body::Failed(e), .. }) => e.clone(),
            Some(Reading { body: Body::Shown, .. }) => String::new(),
        };
        let head = self.reading.as_ref().map(|r| {
            let muted = |s: String| div().text_xs().text_color(theme.muted_foreground).child(s);
            let date = DateTime::from_timestamp(r.date, 0).unwrap_or_default().with_timezone(&Local);
            let subject = if r.subject.is_empty() { t.no_subject.to_string() } else { r.subject.clone() };
            v_flex()
                .p_3()
                .gap_1()
                .bg(theme.background)
                .border_b_1()
                .border_color(theme.border)
                .child(
                    h_flex()
                        .items_start()
                        .gap_2()
                        .child(div().flex_1().min_w_0().font_family(DISPLAY).text_xl().font_weight(FontWeight::BOLD).line_height(relative(1.3)).child(subject))
                        .children(head_actions),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().truncate().text_sm().font_weight(FontWeight::SEMIBOLD).child(r.from.clone()))
                        .when(r.date > 0, |d| d.child(muted(date.format("%Y-%m-%d %H:%M").to_string()).flex_none())),
                )
                .children(remote)
        });
        // gpui 的画面盖在子窗口之上，WebView2 显示时正文区不能画背景
        v_flex().size_full().children(head).child(
            div()
                .flex_1()
                .relative()
                .when(!show, |d| {
                    d.bg(theme.background).flex().flex_col().gap_3().items_center().justify_center().font_family(PROSE).text_color(theme.muted_foreground).child(status).children(picked_actions)
                })
                .child(canvas(place, |_, _, _, _| {}).absolute().size_full()),
        )
    }

    /// 阅读区头部的远程资源提示条：拦下或已加载的域名，及加载图片的按钮
    fn remote(&self, r: &Reading, cx: &mut Context<Self>) -> AnyElement {
        let (t, theme) = (t(cx), cx.theme());
        let blocked = hosts_text(t, r.hosts.iter().filter(|_| !r.images).chain(&r.blocked));
        let loaded = hosts_text(t, r.hosts.iter().filter(|_| r.images));
        let (icon, color) = match (loaded.is_empty(), blocked.is_empty()) {
            (false, _) => (IconName::ShieldAlert, theme.warning),
            (true, false) => (IconName::ShieldBan, theme.primary),
            (true, true) => (IconName::ShieldCheck, theme.success),
        };
        let (bg, fg) = (theme.secondary, theme.muted_foreground);
        let mut lines: Vec<_> = [(t.loaded_from, loaded), (t.blocked_from, blocked)].into_iter().filter(|(_, h)| !h.is_empty()).map(|(l, h)| format!("{l}{h}")).collect();
        if lines.is_empty() {
            lines.push(t.no_remote.into());
        }
        let button = |id: &'static str, label: &'static str, images: bool, trusted: Option<bool>| {
            Button::new(id).link().xsmall().label(label).on_click(cx.listener(move |v, _, _, cx| v.set_images(images, trusted, cx)))
        };
        let trust = !r.addr.is_empty();
        h_flex()
            .mt_1()
            .gap_2()
            .px_2()
            .py_1p5()
            .rounded(R_MD)
            .bg(bg)
            .text_xs()
            .text_color(fg)
            .child(Icon::new(icon).small().text_color(color))
            .child(v_flex().flex_1().min_w_0().children(lines.into_iter().map(|s| div().child(s))))
            // 命令行打开的 .eml 不属于账号，不知道该走哪个代理，不给加载
            .when(r.account != 0 && !r.hosts.is_empty(), |d| {
                d.child(
                    h_flex()
                        .flex_none()
                        .gap_3()
                        .when(!r.images, |d| d.child(button("load-images", t.load_images, true, None)))
                        .when(trust && !r.trusted, |d| d.child(button("trust-sender", t.trust_sender, true, Some(true))))
                        .when(trust && r.trusted, |d| d.child(button("untrust-sender", t.untrust_sender, false, Some(false)))),
                )
            })
            .into_any_element()
    }
}

/// “域名 ×次数”列表，按首次出现的顺序
fn hosts_text<'a>(t: &T, hosts: impl Iterator<Item = &'a String>) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for h in hosts {
        match counts.iter_mut().find(|(x, _)| x == h) {
            Some(c) => c.1 += 1,
            None => counts.push((h, 1)),
        }
    }
    let items: Vec<_> = counts.iter().map(|(h, n)| if *n > 1 { format!("{h} ×{n}") } else { h.to_string() }).collect();
    items.join(t.sep)
}

/// 浏览器打开成功就关对话框，失败留着显示原因
fn opened(dlg: &Entity<LinkDlg>, r: Result<(), String>, window: &mut Window, cx: &mut App) {
    match r {
        Ok(()) => window.close_dialog(cx),
        Err(e) => dlg.update(cx, |d, cx| {
            (d.busy, d.error) = (false, Some(e));
            cx.notify();
        }),
    }
}

fn adspower_dialog(dlg: &Entity<AdsDlg>, view: &WeakEntity<MailPro>, dialog: Dialog, cx: &mut App) -> Dialog {
    let (t, theme) = (t(cx), cx.theme());
    let d = dlg.read(cx);
    let field = |label: &'static str, input: &Entity<InputState>| v_flex().gap_1().child(div().text_sm().child(label)).child(Input::new(input));
    let (dlg, view) = (dlg.clone(), view.clone());
    dialog
        .title(title(t.adspower.trim_end_matches('…')))
        .w(px(440.))
        .child(
            v_flex()
                .gap_3()
                .child(div().font_family(PROSE).text_sm().line_height(relative(1.7)).text_color(theme.muted_foreground).child(t.adspower_hint))
                .child(field(t.profile_id, &d.id))
                .child(field("API key", &d.key))
                .when_some(d.error.clone(), |d, e| d.child(div().text_xs().text_color(theme.danger).child(e))),
        )
        .footer(
            h_flex()
                .justify_end()
                .gap_2()
                .child(Button::new("cancel").label(t.cancel).on_click(|_, window, cx| window.close_dialog(cx)))
                .child(Button::new("ok").primary().label(t.save).on_click(move |_, window, cx| {
                    let d = dlg.read(cx);
                    let (id, key) = (d.id.read(cx).value().trim().to_string(), d.key.read(cx).value().trim().to_string());
                    let error = if id.is_empty() {
                        Some(t.need_profile.to_string())
                    } else if key.is_empty() && !d.saved {
                        Some(t.need_key.to_string())
                    } else if key.is_empty() {
                        None
                    } else {
                        cred::save(browser::KEY, &key).err().map(|e| e.to_string())
                    };
                    if error.is_none() {
                        let _ = view.update(cx, |v, cx| v.set_browser(browser::adspower_setting(&id), cx));
                        return window.close_dialog(cx);
                    }
                    dlg.update(cx, |d, cx| {
                        d.error = error;
                        cx.notify();
                    });
                })),
        )
}

fn link_dialog(dlg: &Entity<LinkDlg>, view: &WeakEntity<MailPro>, dialog: Dialog, cx: &mut App) -> Dialog {
    let (t, theme) = (t(cx), cx.theme());
    let d = dlg.read(cx);
    let l = &d.link;
    let url = link::web(&l.url);
    // 清洗改过地址才给原地址；改得动说明原地址是正常的网页地址
    let original = (!l.unwrapped.is_empty() || !l.removed.is_empty()).then(|| link::web(&d.raw)).flatten();
    let text = url.clone().unwrap_or_else(|| d.raw.strip_prefix("mailto:").unwrap_or(&d.raw).to_string());
    let muted = |s: String| div().text_xs().text_color(theme.muted_foreground).line_clamp(3).child(s);
    let busy = d.busy;
    let open = |id: &'static str, label: &'static str, url: String| {
        let (dlg, view) = (dlg.clone(), view.clone());
        Button::new(id).label(label).disabled(busy).on_click(move |_, window, cx| {
            let Ok(setting) = view.read_with(cx, |v, _| v.browser.clone()) else { return };
            let Some(id) = browser::adspower(&setting).map(str::to_string) else {
                return opened(&dlg, browser::open(&url, &setting), window, cx);
            };
            // AdsPower 可能要先启动环境，放到后台等，期间按钮置灰
            dlg.update(cx, |d, cx| {
                (d.busy, d.error) = (true, None);
                cx.notify();
            });
            let url = url.clone();
            let task = cx.background_spawn(async move { browser::open_adspower(&url, &id) });
            let (dlg, handle) = (dlg.clone(), Window::window_handle(window));
            cx.spawn(async move |cx| {
                let r = task.await;
                let _ = cx.update_window(handle, |_, window, cx| opened(&dlg, r, window, cx));
            })
            .detach();
        })
    };
    let copy = text.clone();
    dialog
        .title(title(t.open_link))
        .w(px(560.))
        .child(
            v_flex()
                .gap_2()
                .child(div().text_sm().line_clamp(6).child(text))
                .when_some(original.clone(), |d, o| d.child(muted(format!("{}{o}", t.original))))
                .when(!l.unwrapped.is_empty(), |d| d.child(muted(format!("{}{}", t.unwrapped, l.unwrapped.join(" → ")))))
                .when(!l.removed.is_empty(), |d| d.child(muted(format!("{}{}", t.removed_params, l.removed.join(t.sep)))))
                .when(url.is_none(), |d| d.child(muted(t.not_web.into())))
                .when_some(d.error.clone(), |d, e| d.child(div().text_xs().text_color(theme.danger).child(format!("{}{e}", t.open_failed)))),
        )
        .footer(
            h_flex()
                .justify_end()
                .gap_2()
                .child(Button::new("cancel").label(t.cancel).on_click(|_, window, cx| window.close_dialog(cx)))
                .child(Button::new("copy").label(t.copy).on_click(move |_, window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                    window.close_dialog(cx);
                }))
                .when_some(original, |d, o| d.child(open("open-original", t.open_original, o)))
                .when_some(url, |d, u| d.child(open("open", if busy { t.opening } else { t.open }, u).primary())),
        )
}

impl Render for MailPro {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let show = matches!(self.reading, Some(Reading { body: Body::Shown, .. })) && !window.has_active_dialog(cx) && self.menu.is_none();
        h_resizable("columns")
            .child(resizable_panel().size(px(240.)).size_range(px(160.)..px(480.)).child(self.sidebar(cx)))
            .child(resizable_panel().size(px(380.)).size_range(px(260.)..px(800.)).child(self.list(cx)))
            .child(resizable_panel().child(self.reader(show, cx)))
    }
}

fn form_dialog(form: &Entity<Form>, view: &WeakEntity<MailPro>, dialog: Dialog, cx: &mut App) -> Dialog {
    let (t, theme) = (t(cx), cx.theme());
    let f = form.read(cx);
    let settings = f.account.is_some();
    let email = f.email.read(cx).value();
    let need_host = !settings && imap::default_host(&email).is_none();
    let password = if imap::default_host(&email).is_some() { t.app_password } else { t.password };
    let direct = f.proxy.read(cx).value().trim().is_empty();
    let field = |label: &'static str, input: &Entity<InputState>| v_flex().gap_1().child(div().text_sm().child(label)).child(Input::new(input));
    let view = view.clone();
    dialog
        .title(title(if settings { t.account_settings } else { t.add_account }))
        .w(px(440.))
        .child(
            v_flex()
                .gap_3()
                .when(!settings, |d| d.child(field(t.email, &f.email)).child(field(password, &f.secret)))
                .when(need_host, |d| d.child(h_flex().gap_2().child(field(t.server, &f.host).flex_1()).child(field(t.port, &f.port).w(px(90.)))))
                .child(field(t.proxy, &f.proxy))
                .when(direct, |d| d.child(div().font_family(PROSE).text_sm().text_color(theme.warning).child(t.direct_warning)))
                .when_some(f.error.clone(), |d, e| d.child(div().text_xs().text_color(theme.danger).child(e))),
        )
        .footer(
            h_flex()
                .justify_end()
                .gap_2()
                .child(Button::new("cancel").label(t.cancel).on_click(|_, window, cx| window.close_dialog(cx)))
                .child(
                    Button::new("ok")
                        .primary()
                        .label(if f.busy { t.signing_in } else if settings { t.save } else { t.add })
                        .disabled(f.busy)
                        .on_click(move |_, window, cx| {
                            let _ = view.update(cx, |v, cx| v.submit(window, cx));
                        }),
                ),
        )
}

/// 对话框标题：衬线粗体
/// 彻底删除选中的 `n` 封前确认
fn delete_dialog(n: usize, view: &WeakEntity<MailPro>, dialog: Dialog, cx: &mut App) -> Dialog {
    let (t, theme) = (t(cx), cx.theme());
    let view = view.clone();
    dialog
        .title(title(t.delete_forever))
        .w(px(400.))
        .child(
            v_flex()
                .gap_1()
                .font_family(PROSE)
                .text_sm()
                .child(format!("{}{n}{}", t.picked, t.messages))
                .child(div().text_color(theme.muted_foreground).child(t.delete_forever_hint)),
        )
        .footer(
            h_flex()
                .justify_end()
                .gap_2()
                .child(Button::new("cancel").label(t.cancel).on_click(|_, window, cx| window.close_dialog(cx)))
                .child(Button::new("ok").danger().label(t.delete_forever).on_click(move |_, window, cx| {
                    window.close_dialog(cx);
                    let _ = view.update(cx, |v, cx| v.move_picked(None, window, cx));
                })),
        )
}

/// 选移动的去处：账号里当前文件夹以外的文件夹
fn move_dialog(folders: &[Folder], view: &WeakEntity<MailPro>, dialog: Dialog, cx: &mut App) -> Dialog {
    let (t, theme) = (t(cx), cx.theme());
    let rows = folders.iter().enumerate().map(|(i, f)| {
        let (f, view) = (f.clone(), view.clone());
        h_flex()
            .id(("to", i))
            .gap_2()
            .px_2()
            .py_1p5()
            .rounded(R_MD)
            .cursor_pointer()
            .hover(|d| d.bg(theme.list_hover))
            .child(Icon::new(folder_icon(&f)).small().text_color(theme.muted_foreground))
            .child(div().flex_1().truncate().text_sm().child(folder_label(t, &f)))
            .on_click(move |_, window, cx| {
                window.close_dialog(cx);
                let _ = view.update(cx, |v, cx| v.move_picked(Some(f.clone()), window, cx));
            })
    });
    dialog.title(title(t.move_to.trim_end_matches('…'))).w(px(360.)).child(v_flex().id("folders").max_h(px(420.)).overflow_y_scroll().children(rows))
}

fn title(s: &str) -> impl IntoElement {
    div().font_family(DISPLAY).text_xl().font_weight(FontWeight::BOLD).child(s.to_string())
}

/// 账号的首字母圆标；色相按账号 id 取黄金分割步进，相邻账号颜色差得开
fn avatar(id: i64, email: &str) -> impl IntoElement {
    div()
        .size(px(20.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(hsla((id as f32 * 0.618_034).fract(), 0.5, 0.45, 1.))
        .text_xs()
        .font_weight(FontWeight::BOLD)
        .text_color(white())
        .child(email.chars().next().unwrap_or('?').to_uppercase().to_string())
}

/// 按 `role` 取图标，Gmail 标签等自建文件夹用普通文件夹图标
fn folder_icon(f: &Folder) -> IconName {
    match f.role.as_deref() {
        Some("inbox") => IconName::Inbox,
        Some("flagged") => IconName::Star,
        Some("important") => IconName::Tag,
        Some("sent") => IconName::Send,
        Some("drafts") => IconName::FilePen,
        Some("all") => IconName::Mails,
        Some("archive") => IconName::Archive,
        Some("junk") => IconName::OctagonAlert,
        Some("trash") => IconName::Trash,
        _ => IconName::Folder,
    }
}

fn folder_label(t: &T, f: &Folder) -> String {
    match f.role.as_deref().and_then(|r| ROLES.iter().position(|x| *x == r)) {
        Some(i) => t.roles[i].to_string(),
        None => folder_name(&f.name),
    }
}

/// 列表里的时间：今天只显示时刻，今年显示月日，更早带年份
fn short_date(t: &T, date: i64) -> String {
    let d = DateTime::from_timestamp(date, 0).unwrap_or_default().with_timezone(&Local);
    let now = Local::now();
    let f = if d.date_naive() == now.date_naive() {
        "%H:%M"
    } else if d.year() == now.year() {
        t.day
    } else {
        t.full_day
    };
    d.format(f).to_string()
}
