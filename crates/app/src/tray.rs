//! 通知区域图标：关掉窗口后缩到这里继续收信；新信用它的气泡通知，Windows 10/11 显示成右下角的通知。
//! 图标挂在一个隐藏窗口上，窗口过程在 gpui 的消息循环里跑（同一线程），事件经通道交给界面。

use crate::i18n::T;
use futures::channel::mpsc::UnboundedSender;
use std::cell::{Cell, RefCell};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

const CLASS: PCWSTR = w!("MailProTray");
/// 图标的回调消息
const WM_TRAY: u32 = WM_APP + 1;
/// 再次启动程序时，新进程叫已在运行的那个显示窗口
const WM_WAKE: u32 = WM_APP + 2;

pub enum Event {
    /// 显示主窗口：点图标、菜单里的“打开”、再次启动程序
    Open,
    /// 点了新信通知
    Clicked,
    Exit,
    /// 资源管理器重启，任务栏重建了，图标要重新加
    Readd,
}

thread_local! {
    static TX: RefCell<Option<UnboundedSender<Event>>> = const { RefCell::new(None) };
    /// 右键菜单的“打开”“退出”，跟界面语言
    static MENU: Cell<[&'static str; 2]> = const { Cell::new(["", ""]) };
    static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
}

pub struct Tray {
    nid: NOTIFYICONDATAW,
    t: &'static T,
    /// 收件箱未读总数，悬停时显示
    unread: u32,
}

impl Tray {
    /// 建隐藏窗口并加上图标。窗口标题是数据库路径，[`wake`] 按它找同一份数据的实例
    pub fn new(key: &str, t: &'static T, tx: UnboundedSender<Event>) -> Tray {
        TX.set(Some(tx));
        MENU.set([t.open, t.exit]);
        unsafe {
            TASKBAR_CREATED.set(RegisterWindowMessageW(w!("TaskbarCreated")));
            RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(proc), lpszClassName: CLASS, ..Default::default() });
            // 普通的隐藏窗口，不是 message-only：后者收不到任务栏重建的广播
            let hwnd = CreateWindowExW(WINDOW_EX_STYLE(0), CLASS, &HSTRING::from(key), WS_OVERLAPPED, 0, 0, 0, 0, None, None, None, None).expect("托盘窗口");
            let mut nid = NOTIFYICONDATAW {
                cbSize: size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
                uCallbackMessage: WM_TRAY,
                hIcon: icon(),
                ..Default::default()
            };
            put(&mut nid.szTip, "MailPro");
            let tray = Tray { nid, t, unread: 0 };
            tray.add();
            tray
        }
    }

    /// 换界面语言：右键菜单和悬停提示跟着换
    pub fn lang(&mut self, t: &'static T) {
        MENU.set([t.open, t.exit]);
        self.t = t;
        self.retip();
    }

    pub fn unread(&mut self, n: u32) {
        if n != self.unread {
            self.unread = n;
            self.retip();
        }
    }

    fn retip(&mut self) {
        let tip = if self.unread == 0 { "MailPro".into() } else { self.t.unread_tip.replace("{n}", &self.unread.to_string()) };
        put(&mut self.nid.szTip, &tip);
        let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &self.nid) };
    }

    pub fn add(&self) {
        let _ = unsafe { Shell_NotifyIconW(NIM_ADD, &self.nid) };
    }

    /// 退出前去掉图标，否则要等鼠标划过才消失
    pub fn remove(&self) {
        let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &self.nid) };
    }

    /// 弹通知，图标用托盘图标
    pub fn notify(&self, title: &str, text: &str) {
        let mut nid = NOTIFYICONDATAW { uFlags: NIF_INFO, dwInfoFlags: NIIF_USER, ..self.nid };
        put(&mut nid.szInfoTitle, title);
        put(&mut nid.szInfo, text);
        let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
    }
}

/// 同一份数据已有实例在运行时叫它显示窗口，返回 true。按数据库路径分，测试用的临时数据目录不会唤起正在用的那个
pub fn wake(key: &str) -> bool {
    unsafe {
        let Ok(hwnd) = FindWindowW(CLASS, &HSTRING::from(key)) else { return false };
        // 新进程刚由用户启动，带着前台权限，让给旧进程，它的窗口才到得了最前
        let _ = AllowSetForegroundWindow(ASFW_ANY);
        PostMessageW(Some(hwnd), WM_WAKE, WPARAM(0), LPARAM(0)).is_ok()
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let e = match msg {
        WM_TRAY => match lp.0 as u32 {
            WM_LBUTTONUP => Event::Open,
            NIN_BALLOONUSERCLICK => Event::Clicked,
            WM_RBUTTONUP => match menu(hwnd) {
                1 => Event::Open,
                2 => Event::Exit,
                _ => return LRESULT(0),
            },
            _ => return LRESULT(0),
        },
        WM_WAKE => Event::Open,
        m if m == TASKBAR_CREATED.get() => Event::Readd,
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    };
    TX.with_borrow(|tx| tx.as_ref().map(|tx| tx.unbounded_send(e)));
    LRESULT(0)
}

/// 右键菜单，返回选中的项：1 打开，2 退出，没选为 0
fn menu(hwnd: HWND) -> u32 {
    let [open, exit] = MENU.get();
    unsafe {
        let m = CreatePopupMenu().unwrap_or_default();
        let _ = AppendMenuW(m, MF_STRING, 1, &HSTRING::from(open));
        let _ = AppendMenuW(m, MF_STRING, 2, &HSTRING::from(exit));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // 菜单的所有者要在前台，点菜单外面菜单才会关
        let _ = SetForegroundWindow(hwnd);
        let id = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, None, hwnd, None).0 as u32;
        let _ = DestroyMenu(m);
        id
    }
}

/// exe 自带的应用图标（资源号 1，见 build.rs），按系统小图标尺寸取
fn icon() -> HICON {
    unsafe {
        let (cx, cy) = (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON));
        GetModuleHandleW(None)
            .ok()
            .and_then(|m| LoadImageW(Some(HINSTANCE(m.0)), PCWSTR(std::ptr::without_provenance(1)), IMAGE_ICON, cx, cy, LR_DEFAULTCOLOR).ok())
            .map(|h| HICON(h.0))
            .unwrap_or_else(|| LoadIconW(None, IDI_APPLICATION).unwrap_or_default())
    }
}

/// 写进定长的 UTF-16 栏位，放不下就截断（不拆开代理对），末尾补 0
fn put(dst: &mut [u16], s: &str) {
    let mut i = 0;
    for c in s.chars() {
        if i + c.len_utf16() >= dst.len() {
            break;
        }
        i += c.encode_utf16(&mut dst[i..]).len();
    }
    dst[i] = 0;
}

/// 关窗口时藏起来，继续收信
pub fn hide(hwnd: HWND) {
    let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
}

pub fn show(hwnd: HWND) {
    let _ = unsafe { ShowWindow(hwnd, SW_SHOW) };
}
