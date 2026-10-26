//! 第二步的验证工具：添加账号、同步、看列表、标已读、挂着 IDLE 看变化；界面在第三步。

use chrono::{DateTime, Local};
use mailpro_core::{cred, smtp};
use mailpro_core::imap::{self, Event, Result, folder_name};
use mailpro_core::store::{self, Account, Folder, Store};
use std::io::{self, Write};
use std::process::ExitCode;

const USAGE: &str = "用法：
  mailpro-cli add <邮箱> [服务器 [端口]]          Gmail 自动填服务器；然后输入密码（不回显）和代理
  mailpro-cli proxy <邮箱> [主机:端口]             设 SOCKS5 代理，不带地址则改回直连
  mailpro-cli sync                                同步全部账号
  mailpro-cli list <邮箱> [文件夹]                 不带文件夹列出文件夹，带则列出最新 50 封
  mailpro-cli read|unread <邮箱> <文件夹> <uid>    标已读 / 未读并写回服务器，连不上则下次同步时写回
  mailpro-cli save <邮箱> <文件夹> <uid> <文件>    下载原始邮件存成 .eml
  mailpro-cli watch                                保持同步，打印每次变化
数据库在 %LOCALAPPDATA%\\MailPro\\mail.db，密码在 Windows 凭据管理器。";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    match rt.block_on(run(args.iter().map(String::as_str).collect())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Vec<&str>) -> Result<()> {
    let db = Store::open(&store::path()?)?;
    match args[..] {
        ["add", email, ref rest @ ..] => add(&db, email, rest).await?,
        ["proxy", email, ref proxy @ ..] if proxy.len() < 2 => db.set_proxy(account(&db, email)?.id, proxy.first().copied())?,
        ["sync"] => {
            for acc in db.accounts()? {
                let mut s = imap::connect(&acc, &cred::load(&acc.email)?).await?;
                imap::sync_all(&mut s, &db, &acc, &mut print(&acc.email)).await?;
            }
        }
        ["list", email] => {
            for f in db.folders(account(&db, email)?.id)? {
                let (total, unread) = db.stats(f.id)?;
                println!("{:<30} {total:>7} 封 {unread:>6} 未读", folder_name(&f.name));
            }
        }
        ["list", email, folder] => {
            for h in db.list(folder_of(&db, email, folder)?.1.id, 50)? {
                let date = DateTime::from_timestamp(h.date, 0).unwrap_or_default().with_timezone(&Local);
                let from = if h.from_name.is_empty() { &h.from_addr } else { &h.from_name };
                let unread = if h.flags & store::SEEN == 0 { '●' } else { ' ' };
                let attach = if h.attach { " 📎" } else { "" };
                println!("{:>7} {unread} {} {from:<24} {}{attach}", h.uid, date.format("%Y-%m-%d %H:%M"), h.subject);
            }
        }
        [op @ ("read" | "unread"), email, folder, uid] => {
            let (acc, f) = folder_of(&db, email, folder)?;
            db.mark(f.id, &[uid.parse()?], store::SEEN, op == "read")?;
            match imap::connect(&acc, &cred::load(&acc.email)?).await {
                Ok(mut s) => drop(imap::sync_folder(&mut s, &db, &f).await?),
                Err(e) => println!("连不上（{e}），已记下，下次同步时写回"),
            }
        }
        ["save", email, folder, uid, file] => {
            let (acc, f) = folder_of(&db, email, folder)?;
            let mut s = imap::connect(&acc, &cred::load(&acc.email)?).await?;
            let raw = imap::fetch_body(&mut s, &db, &f, uid.parse()?).await?.ok_or("服务器上没有这封")?;
            std::fs::write(file, raw)?;
        }
        ["watch"] => {
            let accounts = db.accounts()?;
            let secrets = accounts.iter().map(|a| cred::load(&a.email)).collect::<std::result::Result<Vec<_>, _>>()?;
            let watches = accounts.iter().zip(&secrets).map(|(a, s)| imap::watch(&db, a, s, print(&a.email)));
            futures::future::join_all(watches).await;
        }
        _ => println!("{USAGE}"),
    }
    Ok(())
}

async fn add(db: &Store, email: &str, rest: &[&str]) -> Result<()> {
    if imap::oauth_only(email) {
        return Err("Outlook / Hotmail 只能用 OAuth 登录，等有了 client ID 再支持".into());
    }
    let host = rest.first().copied().or(imap::default_host(email)).ok_or("这个邮箱要写上 IMAP 服务器地址")?;
    let port = rest.get(1).map_or(Ok(993), |p| p.parse())?;
    let secret = imap::tidy_secret(host, &prompt_hidden("密码：")?);
    print!("SOCKS5 代理（如 127.0.0.1:7898，直连留空）：");
    io::stdout().flush()?;
    let mut proxy = String::new();
    io::stdin().read_line(&mut proxy)?;
    let proxy = Some(proxy.trim().to_string()).filter(|p| !p.is_empty());
    let smtp_host = smtp::default_host(host);
    let acc = Account { email: email.into(), host: host.into(), port, proxy, smtp_host, smtp_port: 465, ..Default::default() };
    imap::connect(&acc, &secret).await?.logout().await?;
    cred::save(email, &secret)?;
    db.add_account(&acc)?;
    println!("已添加 {email}，运行 sync 开始同步");
    Ok(())
}

/// 在终端里读一行，不回显；输入是管道时照常读
fn prompt_hidden(label: &str) -> io::Result<String> {
    use windows::Win32::System::Console::*;
    print!("{label}");
    io::stdout().flush()?;
    let mut mode = CONSOLE_MODE::default();
    let console = unsafe { GetStdHandle(STD_INPUT_HANDLE) }
        .ok()
        .filter(|h| unsafe { GetConsoleMode(*h, &mut mode) }.is_ok());
    if let Some(h) = console {
        let _ = unsafe { SetConsoleMode(h, mode & !ENABLE_ECHO_INPUT) };
    }
    let mut line = String::new();
    let read = io::stdin().read_line(&mut line);
    if let Some(h) = console {
        let _ = unsafe { SetConsoleMode(h, mode) };
        println!();
    }
    read?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

fn print(email: &str) -> impl FnMut(Event) + '_ {
    move |e| {
        let now = Local::now().format("%H:%M:%S");
        match e {
            Event::Folders => {}
            Event::Changed(f, c) => {
                println!("{now} {email} {}：新 {}，标记变 {}，删 {}", folder_name(&f.name), c.new, c.flags, c.gone)
            }
            Event::Error(e) => eprintln!("{now} {email} 断开：{e}"),
        }
    }
}

fn account(db: &Store, email: &str) -> Result<Account> {
    let acc = db.accounts()?.into_iter().find(|a| a.email.eq_ignore_ascii_case(email));
    Ok(acc.ok_or_else(|| format!("没有账号 {email}，先 add"))?)
}

/// 文件夹写显示名或服务器原名都行
fn folder_of(db: &Store, email: &str, folder: &str) -> Result<(Account, Folder)> {
    let acc = account(db, email)?;
    let f = db.folders(acc.id)?.into_iter().find(|f| f.name == folder || folder_name(&f.name) == folder);
    let f = f.ok_or_else(|| format!("{email} 没有文件夹 {folder}，先 sync"))?;
    Ok((acc, f))
}
