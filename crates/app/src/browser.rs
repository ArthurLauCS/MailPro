//! 用设置里选的浏览器打开网址。设置存一个字符串：空为系统默认；`adspower:<环境 ID>` 为 AdsPower 的环境；
//! 其余为程序路径，只传网址这一个参数。
//!
//! AdsPower 的环境不能直接运行 SunBrowser.exe 打开：指纹和代理是 AdsPower 启动时带进去的，直接运行会裸连。
//! 所以走它的本地 API：环境没开就让 AdsPower 按自己的配置打开，再从环境的调试端口开新标签页。

use mailpro_core::cred;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, w};

const ADSPOWER: &str = "adspower:";
/// API key 在凭据管理器里的名字
pub const KEY: &str = "adspower";
/// AdsPower 本地 API 的固定端口
const API_PORT: u16 = 50325;

pub fn adspower(setting: &str) -> Option<&str> {
    setting.strip_prefix(ADSPOWER)
}

pub fn adspower_setting(id: &str) -> String {
    format!("{ADSPOWER}{id}")
}

/// 系统默认或指定程序；`url` 须是 [`mailpro_core::link::web`] 写出的地址
pub fn open(url: &str, setting: &str) -> Result<(), String> {
    if !setting.is_empty() {
        return std::process::Command::new(setting).arg(url).spawn().map(drop).map_err(|e| e.to_string());
    }
    let r = unsafe { ShellExecuteW(None, w!("open"), &HSTRING::from(url), None, None, SW_SHOWNORMAL) };
    // 大于 32 为成功，否则是错误码
    if r.0 as isize > 32 { Ok(()) } else { Err(format!("ShellExecute {}", r.0 as isize)) }
}

/// 在 AdsPower 环境 `id` 里开新标签页。环境要启动时会等上几秒到几十秒，不要在界面线程上调用
pub fn open_adspower(url: &str, id: &str) -> Result<(), String> {
    let key = cred::load(KEY).map_err(|_| "AdsPower API key ?")?;
    let api = |cmd: &str| -> Result<Value, String> {
        let v: Value = serde_json::from_str(&get(API_PORT, "GET", &format!("/api/v1/browser/{cmd}?user_id={}", escape(id)), Some(&key))?)
            .map_err(|e| format!("AdsPower: {e}"))?;
        match v["code"].as_i64() {
            Some(0) => Ok(v["data"].clone()),
            _ => Err(format!("AdsPower: {}", v["msg"].as_str().unwrap_or("?"))),
        }
    };
    // 这几个接口共用每秒一次的频率限制
    let pause = || std::thread::sleep(Duration::from_millis(1100));
    let mut env = api("active")?;
    let mut tries = 0;
    loop {
        if env["status"] != "Active" {
            pause();
            env = api("start")?;
        }
        let port = env["debug_port"].as_str().and_then(|p| p.parse().ok()).ok_or("AdsPower: debug_port ?")?;
        // 刚关掉的环境在 AdsPower 收尾的几秒里仍报 Active，给的调试端口已经关了：等它报 Inactive 再拉起。
        // 只在端口连不上时重来：开标签页的请求发出去后失败也不重发，免得重复打开
        if TcpStream::connect(("127.0.0.1", port)).is_ok() || tries == 10 {
            // 调试端口的 /json/new 把整个查询串解码一次当网址
            return get(port, "PUT", &format!("/json/new?{}", escape(url)), None).map(drop);
        }
        tries += 1;
        pause();
        env = api("active")?;
    }
}

/// 除字母数字外全部百分号转义
fn escape(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// 本机明文 HTTP 请求，返回响应正文。浏览器的调试端口不理 Connection: close，回完不断开，
/// 所以正文按 Content-Length 读，没有这个头才读到对方断开
fn get(port: u16, method: &str, path: &str, key: Option<&str>) -> Result<String, String> {
    let req = || -> std::io::Result<(String, String)> {
        let mut s = TcpStream::connect(("127.0.0.1", port))?;
        s.set_read_timeout(Some(Duration::from_secs(90)))?;
        let auth = key.map(|k| format!("Authorization: Bearer {k}\r\n")).unwrap_or_default();
        write!(s, "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{auth}Content-Length: 0\r\nConnection: close\r\n\r\n")?;
        let mut r = BufReader::new(s);
        let mut head = String::new();
        // 读到空行（只有 \r\n）或断开为止
        while r.read_line(&mut head)? > 2 {}
        let len = head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length").then(|| v.trim().parse().ok())?
        });
        let mut body = String::new();
        r.take(len.unwrap_or(u64::MAX)).read_to_string(&mut body)?;
        Ok((head, body))
    };
    let (head, body) = req().map_err(|e| format!("127.0.0.1:{port}: {e}"))?;
    match head.split(' ').nth(1) {
        Some("200") => Ok(body),
        status => Err(format!("127.0.0.1:{port}: HTTP {}", status.unwrap_or("?"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn reads_reply_without_close() {
        // 像浏览器调试端口一样回完不断开：连接留在线程返回值里，join 前不关
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let _ = s.read(&mut [0; 1024]);
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length:7\r\nContent-Type:application/json\r\n\r\n{\"a\":1}").unwrap();
            s
        });
        assert_eq!(get(port, "PUT", "/json/new?x", None).unwrap(), "{\"a\":1}");
        drop(server.join());
    }
}
