//! 对着本地测试服务器走一遍同步：首次拉取、写回已读和星标、另一端改标记 / 删信 / 来新信后再同步、下载正文、
//! 移动和彻底删除、IDLE 实时收新信；另一个用户发信、存草稿。
//! 默认跳过。跑法（GreenMail，明文 IMAP 端口 3143、SMTP 端口 3025，一次一跑，跑完重启服务器清空邮箱）：
//!   java -Dgreenmail.setup.test.imap -Dgreenmail.setup.test.smtp -Dgreenmail.users=t:p@localhost,u:p@localhost -jar greenmail-standalone.jar
//!   MAILPRO_TEST_IMAP=127.0.0.1:3143 MAILPRO_TEST_SMTP=127.0.0.1:3025 cargo test -p mailpro-core --test sync -- --ignored

use futures::TryStreamExt;
use futures::future::{Either, select};
use mailpro_core::imap::{self, Changes, Event, Session};
use mailpro_core::message;
use mailpro_core::smtp::{self, Mail};
use mailpro_core::store::{Account, FLAGGED, SEEN, Store};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::sleep;

fn mail(i: u32) -> String {
    let head = format!("From: =?UTF-8?B?5rWL6K+V?= <a{i}@example.invalid>\r\nSubject: =?UTF-8?B?5rWL6K+V?= {i}\r\n");
    if i == 3 {
        head + "Content-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nhi\r\n--b\r\n\
                Content-Type: text/plain\r\nContent-Disposition: attachment; filename=a.txt\r\n\r\nfile\r\n--b--\r\n"
    } else {
        head + "\r\nbody\r\n"
    }
}

/// 服务器上这封的（已读，星标）
async fn on_server(s: &mut Session<TcpStream>, uid: u32) -> (bool, bool) {
    use async_imap::types::Flag;
    let f: Vec<_> = s.uid_fetch(uid.to_string(), "FLAGS").await.unwrap().try_collect().await.unwrap();
    (f[0].flags().any(|f| matches!(f, Flag::Seen)), f[0].flags().any(|f| matches!(f, Flag::Flagged)))
}

#[test]
#[ignore]
fn syncs_against_server() {
    let addr = std::env::var("MAILPRO_TEST_IMAP").unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let open = || async { imap::login(TcpStream::connect(&addr).await.unwrap(), "t", "p").await.unwrap() };
        // web 扮演另一个客户端（网页版）
        let (mut me, mut web) = (open().await, open().await);
        web.create("&XfJT0ZAB-").await.unwrap();
        for i in 1..=3 {
            web.append("INBOX", None, None, mail(i)).await.unwrap();
        }

        let db = Store::open(":memory:").unwrap();
        let acc = Account { email: "t@localhost".into(), host: "127.0.0.1".into(), port: 3143, ..Default::default() };
        db.add_account(&acc).unwrap();
        let acc = db.accounts().unwrap().remove(0);
        imap::sync_all(&mut me, &db, &acc, &mut |_| {}).await.unwrap();
        let names: Vec<_> = db.folders(acc.id).unwrap().into_iter().map(|f| imap::folder_name(&f.name)).collect();
        assert_eq!(names, ["INBOX", "已发送"]);
        let inbox = || db.folders(acc.id).unwrap().remove(0);
        let list = db.list(inbox().id, 10).unwrap();
        assert_eq!(list.len(), 3);
        assert!(list.iter().all(|h| h.from_name == "测试" && h.subject.starts_with("测试 ") && h.flags == 0));
        assert_eq!(list.iter().filter(|h| h.attach).map(|h| h.uid).collect::<Vec<_>>(), [3]);

        // 本地标已读、加星，同步时写回服务器
        db.mark(inbox().id, &[1], SEEN, true).unwrap();
        db.mark(inbox().id, &[1], FLAGGED, true).unwrap();
        assert_eq!(imap::sync_folder(&mut me, &db, &inbox()).await.unwrap(), Changes::default());
        web.select("INBOX").await.unwrap();
        assert_eq!(on_server(&mut web, 1).await, (true, true));
        assert!(db.pending(inbox().id).unwrap().is_empty());

        // 另一端：2 标已读、3 删掉、来一封新的
        web.uid_store("2", "+FLAGS (\\Seen)").await.unwrap().try_collect::<Vec<_>>().await.unwrap();
        web.uid_store("3", "+FLAGS (\\Deleted)").await.unwrap().try_collect::<Vec<_>>().await.unwrap();
        web.expunge().await.unwrap().try_collect::<Vec<_>>().await.unwrap();
        web.append("INBOX", None, None, mail(4)).await.unwrap();
        let ch = imap::sync_folder(&mut me, &db, &inbox()).await.unwrap();
        assert_eq!(ch, Changes { new: 1, flags: 1, gone: 1 });
        assert_eq!(db.stats(inbox().id).unwrap(), (3, 1));
        assert_eq!(db.list(inbox().id, 10).unwrap()[0].flags & SEEN, 0);

        // 正文：下载后进缓存，且不会顺带标已读
        let raw = imap::fetch_body(&mut me, &db, &inbox(), 4).await.unwrap().unwrap();
        assert!(raw.ends_with(b"body\r\n"));
        assert_eq!(db.body(inbox().id, 4).unwrap().unwrap(), raw);
        assert!(!on_server(&mut web, 4).await.0);

        // 4 移到“已发送”，2 彻底删除；本地由界面先删
        let sent = db.folders(acc.id).unwrap().remove(1);
        db.remove(inbox().id, &[2, 4]).unwrap();
        imap::move_to(&mut me, &inbox(), &[4], Some(&sent)).await.unwrap();
        imap::move_to(&mut me, &inbox(), &[2], None).await.unwrap();
        assert_eq!(web.uid_search("ALL").await.unwrap().into_iter().collect::<Vec<_>>(), [1]);
        assert_eq!(imap::sync_folder(&mut me, &db, &sent).await.unwrap(), Changes { new: 1, ..Default::default() });
        assert_eq!(imap::sync_folder(&mut me, &db, &inbox()).await.unwrap(), Changes::default());

        // IDLE：另一端来新信，本地几秒内跟上。另一端改标记靠真实账号验证：
        // GreenMail 不把其他连接改的标记推给 IDLE 中的连接
        let (tx, rx) = std::sync::mpsc::channel();
        let mut on = |e: Event| {
            if let Event::Changed(_, c) = e {
                tx.send(c).unwrap();
            }
        };
        let watcher = Box::pin(imap::watch_session(me, &db, &acc, &mut on));
        let web_side = Box::pin(async {
            sleep(Duration::from_millis(500)).await; // 等 watcher 首轮同步完进入 IDLE
            web.append("INBOX", None, None, mail(5)).await.unwrap();
            for _ in 0..30 {
                if let Ok(c) = rx.try_recv() {
                    return assert_eq!(c, Changes { new: 1, ..Default::default() });
                }
                sleep(Duration::from_millis(100)).await;
            }
            panic!("3 秒内没等到新信");
        });
        if let Either::Left((r, _)) = select(watcher, web_side).await {
            panic!("watcher 提前结束：{:?}", r.err());
        }
    });
}

#[test]
#[ignore]
fn sends_and_saves_drafts() {
    let (imap_addr, smtp_addr) = (std::env::var("MAILPRO_TEST_IMAP").unwrap(), std::env::var("MAILPRO_TEST_SMTP").unwrap());
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let acc = Account { email: "u@localhost".into(), name: "U".into(), host: "127.0.0.1".into(), port: 3143, ..Default::default() };
        let m = Mail { to: "U <u@localhost>".into(), bcc: "hidden@localhost".into(), subject: "测试 发信".into(), body: "第一行\n第二行".into(), ..Default::default() };
        let b = smtp::build(&acc, &m).unwrap();
        let mut c = smtp::connect(TcpStream::connect(&smtp_addr).await.unwrap()).await.unwrap();
        smtp::deliver(&mut c, "u", "p", false, &b).await.unwrap();

        // 收到的这封：EHLO 报 [127.0.0.1]，没有 Bcc 头
        let mut s = imap::login(TcpStream::connect(&imap_addr).await.unwrap(), "u", "p").await.unwrap();
        s.select("INBOX").await.unwrap();
        let f: Vec<_> = s.fetch("1", "BODY.PEEK[]").await.unwrap().try_collect().await.unwrap();
        let raw = f[0].body().unwrap();
        let text = String::from_utf8_lossy(raw);
        assert!(text.contains("[127.0.0.1]") && !text.contains("Bcc:"), "{text}");
        let o = message::original(raw).unwrap();
        assert_eq!((o.subject.as_str(), o.text.as_str()), ("测试 发信", "第一行\n第二行"));

        // 草稿：存进文件夹，同步后看得到，带 Bcc 头、已读
        s.create("Drafts").await.unwrap();
        let db = Store::open(":memory:").unwrap();
        db.add_account(&acc).unwrap();
        let acc = db.accounts().unwrap().remove(0);
        imap::sync_all(&mut s, &db, &acc, &mut |_| {}).await.unwrap();
        let drafts = db.folders(acc.id).unwrap().into_iter().find(|f| f.name == "Drafts").unwrap();
        imap::append(&mut s, &drafts, &b.copy, "(\\Draft \\Seen)").await.unwrap();
        assert_eq!(imap::sync_folder(&mut s, &db, &drafts).await.unwrap(), Changes { new: 1, ..Default::default() });
        let h = db.list(drafts.id, 1).unwrap().remove(0);
        assert_eq!((h.subject.as_str(), h.flags & SEEN), ("测试 发信", SEEN));
        let raw = imap::fetch_body(&mut s, &db, &drafts, h.uid).await.unwrap().unwrap();
        assert_eq!(message::original(&raw).unwrap().bcc.iter().map(|m| m.email.to_string()).collect::<Vec<_>>(), ["hidden@localhost"]);
    });
}
