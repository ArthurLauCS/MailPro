//! 通知区域图标：关掉窗口后缩到这里继续收信；新信用它的气泡通知，Windows 10/11 显示成右下角的通知。
//! 有新信没看时图标带红点。主窗口的任务栏按钮上显示收件箱未读数，新信到时闪烁。
//! 图标挂在一个隐藏窗口上，窗口过程在 gpui 的消息循环里跑（同一线程），事件经通道交给界面。

use crate::i18n::T;
use futures::channel::mpsc::UnboundedSender;
use std::cell::{Cell, RefCell};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

const CLASS: PCWSTR = w!("MailProTray");
/// 图标的回调消息
const WM_TRAY: u32 = WM_APP + 1;
/// 再次启动程序时，新进程叫已在运行的那个显示窗口
const WM_WAKE: u32 = WM_APP + 2;
/// 通知里的图标边长（像素）
const BALLOON: i32 = 64;

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
    /// 收件箱未读总数，悬停时显示，也显示在任务栏按钮上
    unread: u32,
    /// 托盘图标：平时的，有新信没看时带红点的
    icons: [HICON; 2],
    /// 主窗口和它的任务栏按钮
    main: HWND,
    taskbar: Option<ITaskbarList3>,
}

impl Tray {
    /// 建隐藏窗口并加上图标。窗口标题是数据库路径，[`wake`] 按它找同一份数据的实例；`main` 是主窗口
    pub fn new(key: &str, main: HWND, t: &'static T, tx: UnboundedSender<Event>) -> Tray {
        TX.set(Some(tx));
        MENU.set([t.open, t.exit]);
        unsafe {
            TASKBAR_CREATED.set(RegisterWindowMessageW(w!("TaskbarCreated")));
            RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(proc), lpszClassName: CLASS, ..Default::default() });
            // 普通的隐藏窗口，不是 message-only：后者收不到任务栏重建的广播
            let hwnd = CreateWindowExW(WINDOW_EX_STYLE(0), CLASS, &HSTRING::from(key), WS_OVERLAPPED, 0, 0, 0, 0, None, None, None, None).expect("托盘窗口");
            let small = GetSystemMetrics(SM_CXSMICON);
            let icons = [icon(1, small), icon(2, small)];
            let mut nid = NOTIFYICONDATAW {
                cbSize: size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
                uCallbackMessage: WM_TRAY,
                hIcon: icons[0],
                // 通知里用大图标，小图标放大了是糊的
                hBalloonIcon: icon(1, BALLOON),
                ..Default::default()
            };
            put(&mut nid.szTip, "MailPro");
            let taskbar = CoCreateInstance::<_, ITaskbarList3>(&TaskbarList, None, CLSCTX_INPROC_SERVER).ok().filter(|t| t.HrInit().is_ok());
            let tray = Tray { nid, t, unread: 0, icons, main, taskbar };
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
            self.badge();
        }
    }

    fn retip(&mut self) {
        let tip = if self.unread == 0 { "MailPro".into() } else { self.t.unread_tip.replace("{n}", &self.unread.to_string()) };
        put(&mut self.nid.szTip, &tip);
        let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &self.nid) };
    }

    /// 任务栏按钮上的未读数角标。窗口从托盘显示出来时任务栏按钮是新建的，要再设一次
    pub fn badge(&self) {
        let Some(taskbar) = &self.taskbar else { return };
        unsafe {
            if self.unread == 0 {
                let _ = taskbar.SetOverlayIcon(self.main, HICON::default(), None);
                return;
            }
            // 任务栏自己留一份，设完即可销毁
            let icon = number(self.unread);
            let _ = taskbar.SetOverlayIcon(self.main, icon, &HSTRING::from(self.t.unread_tip.replace("{n}", &self.unread.to_string())));
            let _ = DestroyIcon(icon);
        }
    }

    /// 有新信没看：托盘图标换成带红点的；看过了换回来
    pub fn alert(&mut self, on: bool) {
        let icon = self.icons[on as usize];
        if icon != self.nid.hIcon {
            self.nid.hIcon = icon;
            let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &self.nid) };
        }
    }

    pub fn add(&self) {
        let _ = unsafe { Shell_NotifyIconW(NIM_ADD, &self.nid) };
    }

    /// 退出前去掉图标，否则要等鼠标划过才消失
    pub fn remove(&self) {
        let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &self.nid) };
    }

    /// 弹通知，图标用应用图标的大图；同时闪任务栏按钮，直到窗口回到前台（窗口藏在托盘时没有任务栏按钮）
    pub fn notify(&self, title: &str, text: &str) {
        let flash = FLASHWINFO { cbSize: size_of::<FLASHWINFO>() as u32, hwnd: self.main, dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG, ..Default::default() };
        let _ = unsafe { FlashWindowEx(&flash) };
        let mut nid = NOTIFYICONDATAW { uFlags: NIF_INFO, dwInfoFlags: NIIF_USER | NIIF_LARGE_ICON, ..self.nid };
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

/// exe 自带的图标（资源号 1 应用图标，2 带红点的，见 scripts/icon.py），取 `size` 像素见方的那张
fn icon(id: usize, size: i32) -> HICON {
    unsafe {
        GetModuleHandleW(None)
            .ok()
            .and_then(|m| LoadImageW(Some(HINSTANCE(m.0)), PCWSTR(std::ptr::without_provenance(id)), IMAGE_ICON, size, size, LR_DEFAULTCOLOR).ok())
            .map(|h| HICON(h.0))
            .unwrap_or_else(|| LoadIconW(None, IDI_APPLICATION).unwrap_or_default())
    }
}

/// 任务栏角标：红色圆底上的白色数字，超过 99 显示 99+，按系统小图标尺寸画
fn number(n: u32) -> HICON {
    let text: Vec<u16> = if n > 99 { "99+".into() } else { n.to_string() }.encode_utf16().collect();
    unsafe {
        let size = GetSystemMetrics(SM_CXSMICON);
        let head = BITMAPINFOHEADER { biSize: size_of::<BITMAPINFOHEADER>() as u32, biWidth: size, biHeight: -size, biPlanes: 1, biBitCount: 32, ..Default::default() };
        let mut bits = std::ptr::null_mut();
        let dc = CreateCompatibleDC(None);
        let Ok(color) = CreateDIBSection(Some(dc), &BITMAPINFO { bmiHeader: head, ..Default::default() }, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(dc);
            return HICON::default();
        };
        // 先在全黑的位图上用灰度抗锯齿写白字，每个像素的亮度就是字的覆盖度；位数越多字越小
        let font = CreateFontW(-size * [9, 7, 5][text.len() - 1] / 10, 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, ANTIALIASED_QUALITY, 0, w!("Segoe UI"));
        let old = (SelectObject(dc, color.into()), SelectObject(dc, font.into()));
        SetTextColor(dc, COLORREF(0xFFFFFF));
        SetBkMode(dc, TRANSPARENT);
        let mut rect = RECT { right: size, bottom: size, ..Default::default() };
        DrawTextW(dc, &mut text.clone(), &mut rect, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOCLIP);
        let _ = GdiFlush();
        // 再按到圆心的距离铺红底（边缘半像素抗锯齿），白字按覆盖度叠上去，写成预乘透明度的 BGRA
        let px = std::slice::from_raw_parts_mut(bits as *mut [u8; 4], (size * size) as usize);
        let r = size as f32 / 2.;
        for (i, p) in px.iter_mut().enumerate() {
            let (x, y) = ((i as i32 % size) as f32 + 0.5 - r, (i as i32 / size) as f32 + 0.5 - r);
            let a = (r - (x * x + y * y).sqrt()).clamp(0., 1.);
            let w = p[1] as f32 / 255.;
            let c = |red: u8| ((red as f32 * (1. - w) + 255. * w) * a) as u8;
            *p = [c(0x20), c(0x2D), c(0xFF), (a * 255.) as u8];
        }
        let mask = CreateBitmap(size, size, 1, 1, None);
        let icon = CreateIconIndirect(&ICONINFO { fIcon: true.into(), hbmMask: mask, hbmColor: color, ..Default::default() }).unwrap_or_default();
        SelectObject(dc, old.1);
        SelectObject(dc, old.0);
        let _ = (DeleteObject(font.into()), DeleteObject(color.into()), DeleteObject(mask.into()), DeleteDC(dc));
        icon
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
