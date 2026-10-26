//! IMAP 同步：把文件夹和邮件头拉进本地缓存，写回本地的标记改动，在收件箱上 IDLE 等新变化；移动、删除邮件，
//! 存草稿和已发送。

use crate::auth::{Expired, Secret};
use crate::store::{self, Account, Folder, Header, Store};
use async_imap::extensions::idle::IdleResponse;
use async_imap::imap_proto::{BodyStructure, NameAttribute};
use async_imap::types::{Fetch, Flag, Name};
use futures::TryStreamExt;
use mail_parser::MessageParser;
use std::convert::Infallible;
use std::fmt::Debug;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::time::{Instant, sleep, timeout_at};
use tokio_native_tls::{TlsConnector, TlsStream, native_tls};
use tokio_socks::tcp::Socks5Stream;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// 连接底下的字节流：正式走 TLS，测试可以用明文 TCP 连本地测试服务器
pub trait Io: AsyncRead + AsyncWrite + Unpin + Debug + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Debug + Send> Io for T {}
pub type Session<T = TlsStream<TcpStream>> = async_imap::Session<T>;

/// 全部文件夹多久同步一次；收件箱另靠 IDLE 实时
const POLL: Duration = Duration::from_secs(5 * 60);
/// IDLE 多久没动静就结束再重新发起：有的代理掐断空闲约 60 秒的连接（实测 Clash 上游的 ISP SOCKS5）
const KEEP: Duration = Duration::from_secs(25);
/// 断线重连等待：从 5 秒起每次翻倍，封顶 5 分钟
const RETRY_MIN: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(5 * 60);
/// 一条命令最多带的 UID 数，免得命令行过长
const BATCH: usize = 500;
const HEAD: &str = "(UID FLAGS INTERNALDATE RFC822.SIZE BODYSTRUCTURE BODY.PEEK[HEADER.FIELDS (FROM SUBJECT)])";

#[derive(Debug, Default, PartialEq)]
pub struct Changes {
    pub new: usize,
    pub flags: usize,
    pub gone: usize,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.new + self.flags + self.gone == 0
    }
}

pub enum Event<'a> {
    /// 全量同步开始：已连上，文件夹列表已按服务器更新
    Folders,
    Changed(&'a Folder, Changes),
    /// 连接断了，稍后重连
    Error(String),
    /// Outlook 的授权失效（令牌端点拒了刷新令牌），不再重连，要重新登录
    Expired(String),
}

/// 连上收信服务器登录：密码账号用 LOGIN，Outlook 先拿到访问令牌（可能要经代理换新的）再 XOAUTH2
pub async fn connect(acc: &Account, secret: &Secret) -> Result<Session> {
    let pass = secret.get(&acc.email, acc.proxy.as_deref()).await?;
    let tcp = tcp(acc.proxy.as_deref(), &acc.host, acc.port).await?;
    let tls = TlsConnector::from(native_tls::TlsConnector::new()?).connect(&acc.host, tcp).await?;
    if secret.is_oauth() { xoauth2(tls, &acc.email, &pass).await } else { login(tls, &acc.email, &pass).await }
}

/// 本程序的对外连接都从这里出：设了代理就只经代理连，域名交给代理解析，本机不查 DNS；
/// 代理连不上直接报错，不退回直连
pub async fn tcp(proxy: Option<&str>, host: &str, port: u16) -> Result<TcpStream> {
    Ok(match proxy {
        Some(p) => Socks5Stream::connect(p, (host, port)).await.map_err(|e| format!("SOCKS5 {p}: {e}"))?.into_inner(),
        None => TcpStream::connect((host, port)).await?,
    })
}

pub async fn login<T: Io>(stream: T, user: &str, secret: &str) -> Result<Session<T>> {
    let mut client = async_imap::Client::new(stream);
    client.read_response().await?; // 服务器问候语
    Ok(client.login(user, secret).await.map_err(|(e, _)| e)?)
}

/// 用 OAuth 访问令牌登录（SASL XOAUTH2）
pub async fn xoauth2<T: Io>(stream: T, user: &str, token: &str) -> Result<Session<T>> {
    let mut client = async_imap::Client::new(stream);
    client.read_response().await?; // 服务器问候语
    let sasl = XOAuth2(Some(format!("user={user}\x01auth=Bearer {token}\x01\x01")));
    Ok(client.authenticate("XOAUTH2", sasl).await.map_err(|(e, _)| e)?)
}

/// 头一次回令牌；被拒时有的服务器再发一段错误说明，回空行收尾，服务器随即报失败
struct XOAuth2(Option<String>);

impl async_imap::Authenticator for XOAuth2 {
    type Response = String;
    fn process(&mut self, _: &[u8]) -> String {
        self.0.take().unwrap_or_default()
    }
}

/// 同步账号下全部文件夹：先按服务器增删文件夹，再逐个同步
pub async fn sync_all<T: Io>(s: &mut Session<T>, db: &Store, acc: &Account, on: &mut impl FnMut(Event)) -> Result<()> {
    let names: Vec<_> = s.list(Some(""), Some("*")).await?.try_collect().await?;
    let folders: Vec<_> = names
        .iter()
        .filter(|n| !n.attributes().contains(&NameAttribute::NoSelect))
        .map(|n| (n.name().to_string(), role(n)))
        .collect();
    db.set_folders(acc.id, &folders)?;
    on(Event::Folders);
    for f in db.folders(acc.id)? {
        let ch = sync_folder(s, db, &f).await?;
        if !ch.is_empty() {
            on(Event::Changed(&f, ch));
        }
    }
    Ok(())
}

/// 同步一个文件夹，结束时它处于选中状态：
/// 1. 写回本地待写回的标记改动；
/// 2. 拉 UID 比本地大的新邮件；
/// 3. 首次同步未完成或封数对不上时和服务器对账：删掉服务器已删的，从新到旧分批补拉缺的；
/// 4. 更新已有邮件的标记，服务器支持 CONDSTORE 时只拉上次以后改过的。
pub async fn sync_folder<T: Io>(s: &mut Session<T>, db: &Store, f: &Folder) -> Result<Changes> {
    // 支持 CONDSTORE 的服务器在 SELECT 结果里必带 HIGHESTMODSEQ
    let mb = s.select(&f.name).await?;
    let validity = mb.uid_validity.unwrap_or(0);
    let (mut modseq, mut complete) = (f.modseq, f.complete);
    if f.uidvalidity != Some(validity) {
        db.reset_folder(f.id, validity)?;
        (modseq, complete) = (None, false);
    }
    write_pending(s, db, f).await?;

    let mut ch = Changes::default();
    let max = db.max_uid(f.id)?;
    if complete && mb.uid_next.is_none_or(|n| n > max + 1) {
        ch.new += fetch_headers(s, db, f.id, &format!("{}:*", max + 1)).await?;
    }
    if !complete || db.stats(f.id)?.0 != mb.exists {
        let server = s.uid_search("ALL").await?;
        let local = db.uids(f.id)?;
        ch.gone += db.remove(f.id, &local.difference(&server).copied().collect::<Vec<_>>())?;
        let mut missing: Vec<u32> = server.difference(&local).copied().collect();
        missing.sort_unstable_by(|a, b| b.cmp(a));
        for chunk in missing.chunks(BATCH) {
            ch.new += fetch_headers(s, db, f.id, &uid_set(chunk)).await?;
        }
    }

    // 本地原本是空的：邮件头刚连标记一起拉下来，不用再拉标记
    if max > 0 && mb.exists > 0 {
        let query = match (modseq, mb.highest_modseq) {
            (Some(old), Some(now)) if old == now => None,
            (Some(old), Some(_)) => Some(format!("(UID FLAGS) (CHANGEDSINCE {old})")),
            _ => Some("(UID FLAGS)".to_string()),
        };
        if let Some(q) = query {
            let fetched: Vec<Fetch> = s.uid_fetch("1:*", q).await?.try_collect().await?;
            let flags: Vec<(u32, u8)> = fetched.iter().filter_map(|m| Some((m.uid?, flag_bits(m)))).collect();
            ch.flags += db.set_flags(f.id, &flags)?;
        }
    }
    db.finish_folder(f.id, mb.highest_modseq)?;
    Ok(ch)
}

/// 只写回本地的标记改动，不做其他同步；界面上改了标记后马上调
pub async fn flush<T: Io>(s: &mut Session<T>, db: &Store, f: &Folder) -> Result<()> {
    s.select(&f.name).await?;
    write_pending(s, db, f).await
}

/// 把待写回的标记改动写回服务器，调用前文件夹要已选中
async fn write_pending<T: Io>(s: &mut Session<T>, db: &Store, f: &Folder) -> Result<()> {
    let pending = db.pending(f.id)?;
    for (flag, name) in [(store::SEEN, "\\Seen"), (store::FLAGGED, "\\Flagged"), (store::ANSWERED, "\\Answered")] {
        for on in [true, false] {
            let uids: Vec<u32> = pending.iter().filter(|p| p.1 & flag != 0 && (p.2 & flag != 0) == on).map(|p| p.0).collect();
            let op = format!("{}FLAGS.SILENT ({name})", if on { '+' } else { '-' });
            for chunk in uids.chunks(BATCH) {
                s.uid_store(uid_set(chunk), &op).await?.try_for_each(|_| async { Ok(()) }).await?;
                db.clear_pending(f.id, chunk, flag, on)?;
            }
        }
    }
    Ok(())
}

/// 把 `f` 里的这些邮件移到 `to`，`to` 为 None 是彻底删除。服务器不支持 MOVE 时退回
/// 复制 + 标删除 + UID EXPUNGE，只清掉这几封，不连带别的客户端标了删除的邮件（要 UIDPLUS）
pub async fn move_to<T: Io>(s: &mut Session<T>, f: &Folder, uids: &[u32], to: Option<&Folder>) -> Result<()> {
    let caps = s.capabilities().await?;
    let (mv, uidplus) = (caps.has_str("MOVE"), caps.has_str("UIDPLUS"));
    s.select(&f.name).await?;
    for chunk in uids.chunks(BATCH) {
        let set = uid_set(chunk);
        match to {
            Some(to) if mv => s.uid_mv(&set, &to.name).await?,
            _ if !uidplus => return Err("服务器不支持 MOVE 和 UIDPLUS，没法只移走这几封".into()),
            to => {
                if let Some(to) = to {
                    s.uid_copy(&set, &to.name).await?;
                }
                s.uid_store(&set, "+FLAGS.SILENT (\\Deleted)").await?.try_for_each(|_| async { Ok(()) }).await?;
                s.uid_expunge(&set).await?.try_for_each(|_| async { Ok(()) }).await?;
            }
        }
    }
    Ok(())
}

/// 把一封存进文件夹，`flags` 带括号，如 `(\Draft \Seen)`
pub async fn append<T: Io>(s: &mut Session<T>, f: &Folder, raw: &[u8], flags: &str) -> Result<()> {
    Ok(s.append(&f.name, Some(flags), None, raw).await?)
}

/// 一封邮件的原始内容：先查缓存，没有再下载。BODY.PEEK 不会顺带把邮件标成已读。
pub async fn fetch_body<T: Io>(s: &mut Session<T>, db: &Store, f: &Folder, uid: u32) -> Result<Option<Vec<u8>>> {
    if let Some(raw) = db.body(f.id, uid)? {
        return Ok(Some(raw));
    }
    s.select(&f.name).await?;
    let fetched: Vec<Fetch> = s.uid_fetch(uid.to_string(), "BODY.PEEK[]").await?.try_collect().await?;
    let Some(raw) = fetched.iter().filter(|m| m.uid == Some(uid)).find_map(|m| m.body()) else {
        return Ok(None);
    };
    db.put_body(f.id, uid, raw)?;
    Ok(Some(raw.to_vec()))
}

/// 一直保持同步：每 [`POLL`] 同步全部文件夹，其间在收件箱上 IDLE（每 [`KEEP`] 重新发起一次，连接不会闲着），收件箱一有变化立即同步。
/// 断线后等 [`RETRY_MIN`] 起翻倍、封顶 [`RETRY_MAX`] 再重连，一直重试；只有 Outlook 授权失效时停下。
pub async fn watch(db: &Store, acc: &Account, secret: &Secret, mut on: impl FnMut(Event)) {
    let mut delay = RETRY_MIN;
    loop {
        let err = match connect(acc, secret).await {
            Ok(s) => {
                delay = RETRY_MIN;
                let Err(e) = watch_session(s, db, acc, &mut on).await;
                e
            }
            Err(e) => e,
        };
        // 重连也换不到令牌，等用户重新登录
        if err.is::<Expired>() {
            return on(Event::Expired(err.to_string()));
        }
        on(Event::Error(err.to_string()));
        sleep(delay).await;
        delay = (delay * 2).min(RETRY_MAX);
    }
}

/// [`watch`] 的一次连接：一直同步到出错为止
pub async fn watch_session<T: Io>(
    mut s: Session<T>,
    db: &Store,
    acc: &Account,
    on: &mut impl FnMut(Event),
) -> Result<Infallible> {
    loop {
        sync_all(&mut s, db, acc, on).await?;
        let next = Instant::now() + POLL;
        s.select("INBOX").await?;
        loop {
            let mut idle = s.idle();
            idle.init().await?;
            let (wait, _stop) = idle.wait_with_timeout(KEEP);
            // 服务器的保活消息会重置 wait 自己的超时，所以另设截止时间
            let woke = timeout_at(next, wait).await;
            s = idle.done().await?;
            let Ok(r) = woke else { break };
            // 安静了 KEEP：重新 IDLE；否则是新数据，或连接被关（下面的同步会报错，转去重连）
            if matches!(r?, IdleResponse::Timeout) {
                continue;
            }
            let inbox = db.folders(acc.id)?.into_iter().find(|f| f.name.eq_ignore_ascii_case("INBOX"));
            let inbox = inbox.ok_or("服务器没有收件箱")?;
            let ch = sync_folder(&mut s, db, &inbox).await?;
            if !ch.is_empty() {
                on(Event::Changed(&inbox, ch));
            }
        }
    }
}

/// 拉一批邮件头存进本地，返回新增封数
async fn fetch_headers<T: Io>(s: &mut Session<T>, db: &Store, folder: i64, set: &str) -> Result<usize> {
    let fetched: Vec<Fetch> = s.uid_fetch(set, HEAD).await?.try_collect().await?;
    Ok(db.put_headers(folder, &fetched.iter().filter_map(|m| header(folder, m)).collect::<Vec<_>>())?)
}

fn header(folder: i64, m: &Fetch) -> Option<Header> {
    let head = m.header().and_then(|h| MessageParser::new().parse_headers(h));
    let from = head.as_ref().and_then(|h| h.from()).and_then(|l| l.first());
    Some(Header {
        folder,
        uid: m.uid?,
        flags: flag_bits(m),
        date: m.internal_date().map_or(0, |d| d.timestamp()),
        size: m.size.unwrap_or(0),
        attach: m.bodystructure().is_some_and(has_attachment),
        from_name: from.and_then(|a| a.name()).unwrap_or_default().into(),
        from_addr: from.and_then(|a| a.address()).unwrap_or_default().into(),
        subject: head.as_ref().and_then(|h| h.subject()).unwrap_or_default().into(),
    })
}

fn flag_bits(m: &Fetch) -> u8 {
    m.flags().fold(0, |bits, f| {
        bits | match f {
            Flag::Seen => store::SEEN,
            Flag::Flagged => store::FLAGGED,
            Flag::Answered => store::ANSWERED,
            _ => 0,
        }
    })
}

/// 只认 Content-Disposition: attachment；没写 disposition 的内嵌图片不算
fn has_attachment(b: &BodyStructure) -> bool {
    match b {
        BodyStructure::Multipart { bodies, .. } => bodies.iter().any(has_attachment),
        BodyStructure::Basic { common, .. } | BodyStructure::Text { common, .. } | BodyStructure::Message { common, .. } => {
            common.disposition.as_ref().is_some_and(|d| d.ty.eq_ignore_ascii_case("attachment"))
        }
    }
}

/// 文件夹的特殊用途（RFC 6154 和 Gmail 的 `\Important`），见 [`Folder::role`]
fn role(n: &Name) -> Option<&'static str> {
    if n.name().eq_ignore_ascii_case("INBOX") {
        return Some("inbox");
    }
    n.attributes().iter().find_map(|a| match a {
        NameAttribute::All => Some("all"),
        NameAttribute::Archive => Some("archive"),
        NameAttribute::Drafts => Some("drafts"),
        NameAttribute::Flagged => Some("flagged"),
        NameAttribute::Junk => Some("junk"),
        NameAttribute::Sent => Some("sent"),
        NameAttribute::Trash => Some("trash"),
        NameAttribute::Extension(e) if e.eq_ignore_ascii_case("\\Important") => Some("important"),
        _ => None,
    })
}

fn uid_set(uids: &[u32]) -> String {
    uids.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
}

fn domain(email: &str) -> String {
    email.rsplit('@').next().unwrap_or_default().to_ascii_lowercase()
}

/// Gmail、Outlook 的服务器是固定的：(收信服务器, 发信服务器, 发信端口)，收信端口都是 993；
/// 其他邮箱返回 None，要手填
pub fn preset(email: &str) -> Option<(&'static str, &'static str, u16)> {
    if ["gmail.com", "googlemail.com"].contains(&domain(email).as_str()) {
        return Some(("imap.gmail.com", "smtp.gmail.com", 465));
    }
    oauth_only(email).then_some(("outlook.office365.com", "smtp-mail.outlook.com", 587))
}

/// Outlook / Hotmail 只收 OAuth 登录（XOAUTH2），在浏览器里授权，见 [`crate::auth`]
pub fn oauth_only(email: &str) -> bool {
    let d = domain(email);
    ["outlook.", "hotmail.", "live.", "msn."].iter().any(|p| d.starts_with(p))
}

/// 整理粘贴进来的密码：Google 显示应用专用密码时每 4 位隔一个空格，密码本身不含空格
pub fn tidy_secret(host: &str, secret: &str) -> String {
    if host == "imap.gmail.com" { secret.replace(' ', "") } else { secret.to_string() }
}

/// 文件夹显示名：解开 modified UTF-7（RFC 3501 5.1.3），如 `[Gmail]/&XfJT0ZCuTvY-` → `[Gmail]/已发邮件`
pub fn folder_name(raw: &str) -> String {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(i) = rest.find('&') {
        let Some(len) = rest[i..].find('-') else { break };
        out.push_str(&rest[..i]);
        let b64 = &rest[i + 1..i + len];
        if b64.is_empty() {
            out.push('&');
        }
        // 修改版 base64：`,` 代替 `/`，不补 `=`；解出来是 UTF-16BE
        let (mut bits, mut n, mut units) = (0u32, 0, Vec::new());
        for c in b64.bytes() {
            let v = match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' => 62,
                b',' => 63,
                _ => continue,
            };
            bits = bits << 6 | v as u32;
            n += 6;
            if n >= 16 {
                n -= 16;
                units.push((bits >> n) as u16);
                bits &= (1 << n) - 1;
            }
        }
        out.extend(char::decode_utf16(units).map(|c| c.unwrap_or('\u{FFFD}')));
        rest = &rest[i + len + 1..];
    }
    out + rest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_folder_names() {
        assert_eq!(folder_name("INBOX"), "INBOX");
        assert_eq!(folder_name("[Gmail]/&XfJT0ZCuTvY-"), "[Gmail]/已发邮件");
        assert_eq!(folder_name("&XfJT0ZAB-/a&-b"), "已发送/a&b");
    }

    #[test]
    fn xoauth2_login() {
        use base64::Engine;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            // 假服务器：第一次收下令牌；第二次像 Gmail 那样先发一段错误说明，等客户端回空行再报失败
            for ok in [true, false] {
                let (client, server) = tokio::io::duplex(4096);
                let fake = async move {
                    let mut s = BufReader::new(server);
                    let mut line = String::new();
                    s.write_all(b"* OK ready\r\n").await.unwrap();
                    s.read_line(&mut line).await.unwrap();
                    let tag = line.split(' ').next().unwrap().to_string();
                    assert_eq!(&line[tag.len()..], " AUTHENTICATE XOAUTH2\r\n");
                    s.write_all(b"+ \r\n").await.unwrap();
                    line.clear();
                    s.read_line(&mut line).await.unwrap();
                    let sasl = base64::engine::general_purpose::STANDARD.decode(line.trim_end()).unwrap();
                    assert_eq!(sasl, b"user=a@outlook.com\x01auth=Bearer tok\x01\x01");
                    if !ok {
                        s.write_all(b"+ eyJzdGF0dXMiOiI0MDAifQ==\r\n").await.unwrap();
                        line.clear();
                        s.read_line(&mut line).await.unwrap();
                        assert_eq!(line, "\r\n");
                    }
                    let reply = if ok { "OK AUTHENTICATE completed." } else { "NO AUTHENTICATE failed." };
                    s.write_all(format!("{tag} {reply}\r\n").as_bytes()).await.unwrap();
                };
                let (r, ()) = futures::join!(xoauth2(client, "a@outlook.com", "tok"), fake);
                assert_eq!(r.is_ok(), ok);
            }
        });
        assert_eq!(preset("a@Hotmail.com"), Some(("outlook.office365.com", "smtp-mail.outlook.com", 587)));
        assert_eq!(preset("a@x.com"), None);
    }

    #[test]
    fn decodes_gb2312_headers() {
        let head = MessageParser::new().parse_headers(b"From: =?gb2312?B?sf7hqiDB9Q==?= <a@x>\r\n\r\n").unwrap();
        assert_eq!(head.from().and_then(|l| l.first()).and_then(|a| a.name()), Some("炳岐 刘"));
    }
}
