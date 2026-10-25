//! 对着本地 IMAP 测试服务器走一遍同步：首次拉取、写回已读、另一端改标记 / 删信 / 来新信后再同步、下载正文、IDLE 实时收新信。
//! 默认跳过。跑法（GreenMail，明文 IMAP 端口 3143，一次一跑，跑完重启服务器清空邮箱）：
//!   java -Dgreenmail.setup.test.imap -Dgreenmail.users=t:p@localhost -jar greenmail-standalone.jar
//!   MAILPRO_TEST_IMAP=127.0.0.1:3143 cargo test -p mailpro-core --test sync -- --ignored

use futures::TryStreamExt;
use futures::future::{Either, select};
use mailpro_core::imap::{self, Changes, Event, Session};
use mailpro_core::store::{Account, SEEN, Store};
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

async fn seen_on_server(s: &mut Session<TcpStream>, uid: u32) -> bool {
    let f: Vec<_> = s.uid_fetch(uid.to_string(), "FLAGS").await.unwrap().try_collect().await.unwrap();
    f[0].flags().any(|f| matches!(f, async_imap::types::Flag::Seen))
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
        let acc = Account { id: 0, email: "t@localhost".into(), host: "127.0.0.1".into(), port: 3143, proxy: None };
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

        // 本地标已读，同步时写回服务器
        db.mark_seen(inbox().id, 1, true).unwrap();
        assert_eq!(imap::sync_folder(&mut me, &db, &inbox()).await.unwrap(), Changes::default());
        web.select("INBOX").await.unwrap();
        assert!(seen_on_server(&mut web, 1).await);
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
        assert!(!seen_on_server(&mut web, 4).await);

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
