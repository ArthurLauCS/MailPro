//! 后台同步：单独一个线程跑 tokio。每个账号两条连接：一条常驻同步（IDLE + 定时同步），
//! 一条按需连上，取正文、写回标记、移动删除、存草稿和已发送，不排在同步后面。界面只发 [`Cmd`]、收 [`Event`]，
//! 邮件头由界面自己开数据库连接读。发信、代取允许加载的远程图片也在这里，各开新连接。

use crate::http;
use crate::imap::{self, Result, Session};
use crate::message::{self, Message};
use crate::smtp::{self, Built};
use crate::store::{Account, Folder, Store};
use crate::cred;
use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::channel::oneshot;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;
use tokio::task::{JoinHandle, LocalSet, spawn_local};

pub enum Cmd {
    /// 取一封正文并解析，缓存里有就不联网
    Body { account: i64, folder: Folder, uid: u32 },
    /// 本地的标记改动写回服务器
    Flush { account: i64, folder: Folder },
    /// 移到 `to`，None 为彻底删除。界面已先从本地删掉；失败时重新同步原文件夹，邮件回来
    Move { account: i64, folder: Folder, uids: Vec<u32>, to: Option<Folder> },
    /// 试登录，通过才存密码、存账号、开始同步；密码先经 [`imap::tidy_secret`]
    Add(Account, String),
    /// 账号设置（代理）改了，按新设置重连
    Restart(i64),
    /// 经代理取一张远程图片，回 (Content-Type, 内容)，失败回 None
    Image { url: String, proxy: Option<String>, reply: oneshot::Sender<Option<(String, Vec<u8>)>> },
    /// 经 SMTP 发出一封，`sent` 给定时再存一份进去（Gmail 自己会存）；回发信的结果
    Send { account: i64, mail: Built, sent: Option<Folder>, reply: Reply },
    /// 把一封存进文件夹（草稿、已发送），再同步该文件夹；`reply` 给定时回结果，否则失败时发 [`Event::Failed`]
    Append { account: i64, folder: Folder, raw: Vec<u8>, flags: &'static str, reply: Option<Reply> },
}

/// 发信、存草稿的结果，失败为原因
pub type Reply = oneshot::Sender<std::result::Result<(), String>>;

pub enum Event {
    /// 已连上，文件夹列表已按服务器更新
    Folders(i64),
    Changed { account: i64, folder: i64 },
    Body { folder: i64, uid: u32, msg: std::result::Result<Message, BodyError> },
    /// 连接断了，稍后自动重连；account 为 0 是数据库打不开
    Error { account: i64, msg: String },
    /// 添加账号的结果：账号 id 或失败原因
    Added(std::result::Result<i64, String>),
    /// 移动、删除、存已发送失败的原因
    Failed(String),
}

pub enum BodyError {
    /// 服务器上已没有这封
    Gone,
    Unparsable,
    /// 账号没在同步（取不到密码）
    Offline,
    Net(String),
}

enum Job {
    Body(Folder, u32),
    Flush(Folder),
    Move(Folder, Vec<u32>, Option<Folder>),
    Append(Folder, Vec<u8>, &'static str, Option<Reply>),
}

struct Running {
    sync: JoinHandle<()>,
    worker: JoinHandle<()>,
    jobs: UnboundedSender<Job>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.sync.abort();
        self.worker.abort();
    }
}

/// 起后台线程，同步库里的全部账号；返回发命令的口和收事件的口
pub fn spawn(path: String) -> (UnboundedSender<Cmd>, UnboundedReceiver<Event>) {
    let (cmd_tx, cmd_rx) = unbounded();
    let (tx, rx) = unbounded();
    let again = cmd_tx.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio");
        LocalSet::new().block_on(&rt, run(path, cmd_rx, again, tx));
    });
    (cmd_tx, rx)
}

async fn run(path: String, mut cmds: UnboundedReceiver<Cmd>, again: UnboundedSender<Cmd>, tx: UnboundedSender<Event>) {
    let db = match Store::open(&path) {
        Ok(db) => Rc::new(db),
        Err(e) => return drop(tx.unbounded_send(Event::Error { account: 0, msg: e.to_string() })),
    };
    let mut running = HashMap::new();
    for acc in db.accounts().unwrap_or_default() {
        start(&db, acc, &mut running, &tx);
    }
    while let Some(cmd) = cmds.next().await {
        let (account, job) = match cmd {
            Cmd::Body { account, folder, uid } => (account, Job::Body(folder, uid)),
            Cmd::Flush { account, folder } => (account, Job::Flush(folder)),
            Cmd::Move { account, folder, uids, to } => (account, Job::Move(folder, uids, to)),
            Cmd::Add(acc, secret) => {
                let (db, tx, again) = (db.clone(), tx.clone(), again.clone());
                spawn_local(async move {
                    let r = add(&db, &acc, &secret).await.map_err(|e| e.to_string());
                    if let Ok(id) = r {
                        let _ = again.unbounded_send(Cmd::Restart(id));
                    }
                    let _ = tx.unbounded_send(Event::Added(r));
                });
                continue;
            }
            Cmd::Restart(id) => {
                running.remove(&id);
                if let Some(acc) = db.accounts().unwrap_or_default().into_iter().find(|a| a.id == id) {
                    start(&db, acc, &mut running, &tx);
                }
                continue;
            }
            Cmd::Image { url, proxy, reply } => {
                spawn_local(async move {
                    let r = tokio::time::timeout(Duration::from_secs(30), http::get(&url, proxy.as_deref())).await;
                    let r = r.unwrap_or_else(|_| Err("timeout".into())).inspect_err(|e| eprintln!("image {url}: {e}"));
                    let _ = reply.send(r.ok());
                });
                continue;
            }
            Cmd::Send { account, mail, sent, reply } => {
                let Some(acc) = db.accounts().unwrap_or_default().into_iter().find(|a| a.id == account) else { continue };
                let again = again.clone();
                spawn_local(async move {
                    let r = send(&acc, &mail).await.map_err(|e| e.to_string());
                    if let (Ok(()), Some(folder)) = (&r, sent) {
                        let _ = again.unbounded_send(Cmd::Append { account, folder, raw: mail.copy, flags: "(\\Seen)", reply: None });
                    }
                    let _ = reply.send(r);
                });
                continue;
            }
            Cmd::Append { account, folder, raw, flags, reply } => (account, Job::Append(folder, raw, flags, reply)),
        };
        match (running.get(&account), job) {
            (Some(r), job) => drop(r.jobs.unbounded_send(job)),
            // 正文请求、存草稿总要有回音，界面才不会一直显示加载中；没写回的标记改动留在库里；
            // 移动只在账号连着时才发
            (None, Job::Body(f, uid)) => drop(tx.unbounded_send(Event::Body { folder: f.id, uid, msg: Err(BodyError::Offline) })),
            (None, Job::Append(.., Some(reply))) => drop(reply.send(Err("not connected".into()))),
            (None, _) => {}
        }
    }
}

async fn add(db: &Store, acc: &Account, secret: &str) -> Result<i64> {
    let secret = imap::tidy_secret(&acc.host, secret);
    imap::connect(acc, &secret).await?.logout().await?;
    cred::save(&acc.email, &secret)?;
    Ok(db.add_account(acc)?)
}

fn start(db: &Rc<Store>, acc: Account, running: &mut HashMap<i64, Running>, tx: &UnboundedSender<Event>) {
    let id = acc.id;
    let secret = match cred::load(&acc.email) {
        Ok(s) => s,
        Err(e) => return drop(tx.unbounded_send(Event::Error { account: id, msg: e.to_string() })),
    };
    let (jobs, rx) = unbounded();
    let acc = Rc::new(acc);
    let (db2, acc2, secret2, tx2) = (db.clone(), acc.clone(), secret.clone(), tx.clone());
    let worker = spawn_local(async move { worker(&db2, &acc2, &secret2, rx, &tx2).await });
    let (db, tx) = (db.clone(), tx.clone());
    let sync = spawn_local(async move {
        imap::watch(&db, &acc, &secret, |e| {
            let _ = tx.unbounded_send(match e {
                imap::Event::Folders => Event::Folders(id),
                imap::Event::Changed(f, _) => Event::Changed { account: id, folder: f.id },
                imap::Event::Error(msg) => Event::Error { account: id, msg },
            });
        })
        .await
    });
    running.insert(id, Running { sync, worker, jobs });
}

async fn worker(db: &Store, acc: &Account, secret: &str, mut rx: UnboundedReceiver<Job>, tx: &UnboundedSender<Event>) {
    let mut slot = None;
    while let Some(first) = rx.next().await {
        let mut jobs = vec![first];
        while let Ok(j) = rx.try_recv() {
            jobs.push(j);
        }
        // 连着点了好几封时只取最后点的那封，前面的已经不显示了
        let last = jobs.iter().rposition(|j| matches!(j, Job::Body(..)));
        for (i, job) in jobs.into_iter().enumerate() {
            match job {
                Job::Body(f, uid) if Some(i) == last => {
                    let raw = match db.body(f.id, uid) {
                        Ok(Some(raw)) => Ok(Some(raw)),
                        _ => with_session(&mut slot, acc, secret, async |s| imap::fetch_body(s, db, &f, uid).await).await,
                    };
                    let msg = match raw {
                        Ok(Some(raw)) => message::parse(&raw).ok_or(BodyError::Unparsable),
                        Ok(None) => Err(BodyError::Gone),
                        Err(e) => Err(BodyError::Net(e.to_string())),
                    };
                    let _ = tx.unbounded_send(Event::Body { folder: f.id, uid, msg });
                }
                Job::Body(..) => {}
                // 失败不用管：改动还记在库里，同步这个文件夹时会写回
                Job::Flush(f) => drop(with_session(&mut slot, acc, secret, async |s| imap::flush(s, db, &f).await).await),
                Job::Move(f, uids, to) => {
                    let r = with_session(&mut slot, acc, secret, async |s| imap::move_to(s, &f, &uids, to.as_ref()).await).await;
                    if let Err(e) = &r {
                        let _ = tx.unbounded_send(Event::Failed(e.to_string()));
                    }
                    // 成功时同步目标文件夹，移过去的马上看得到；失败时同步原文件夹，本地先删掉的邮件回来
                    if let Some(f) = if r.is_ok() { to } else { Some(f) } {
                        resync(&mut slot, db, acc, secret, &f, tx).await;
                    }
                }
                Job::Append(f, raw, flags, reply) => {
                    let r = with_session(&mut slot, acc, secret, async |s| imap::append(s, &f, &raw, flags).await).await;
                    if r.is_ok() {
                        resync(&mut slot, db, acc, secret, &f, tx).await;
                    }
                    match (reply, r) {
                        (Some(reply), r) => drop(reply.send(r.map_err(|e| e.to_string()))),
                        (None, Err(e)) => drop(tx.unbounded_send(Event::Failed(e.to_string()))),
                        (None, Ok(())) => {}
                    }
                }
            }
        }
    }
}

/// 同步一个文件夹，有变化就告诉界面
async fn resync(slot: &mut Option<Session>, db: &Store, acc: &Account, secret: &str, f: &Folder, tx: &UnboundedSender<Event>) {
    if let Ok(ch) = with_session(slot, acc, secret, async |s| imap::sync_folder(s, db, f).await).await
        && !ch.is_empty()
    {
        let _ = tx.unbounded_send(Event::Changed { account: acc.id, folder: f.id });
    }
}

/// 取出密码发信；连接卡住时 60 秒后放弃，写信框不会一直显示发送中
async fn send(acc: &Account, mail: &Built) -> Result<()> {
    let secret = cred::load(&acc.email)?;
    tokio::time::timeout(Duration::from_secs(60), smtp::send(acc, &secret, mail)).await.map_err(|_| "timeout")?
}

/// 用手上的连接执行；失败就丢掉它，换新连接再试一次（空闲久了服务器会断开）
async fn with_session<T>(
    slot: &mut Option<Session>,
    acc: &Account,
    secret: &str,
    mut op: impl AsyncFnMut(&mut Session) -> Result<T>,
) -> Result<T> {
    if let Some(s) = slot.as_mut() {
        if let Ok(v) = op(s).await {
            return Ok(v);
        }
    }
    *slot = None;
    let r = op(slot.insert(imap::connect(acc, secret).await?)).await;
    if r.is_err() {
        *slot = None;
    }
    r
}
