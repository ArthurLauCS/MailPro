//! 发信：用 lettre 拼成纯文本邮件，经邮箱的代理（[`imap::tcp`]）连 SMTP 发出。587 端口明文连上后
//! STARTTLS 升级（Outlook），其他端口隐式 TLS；密码账号 PLAIN / LOGIN 登录，Outlook 用 XOAUTH2。
//! 发信头脱敏：EHLO 报 `[127.0.0.1]` 不报主机名，Message-ID 是随机串加发件人的域名，Date 用 UTC，
//! 不带 User-Agent / X-Mailer。

use crate::auth::Secret;
use crate::imap::{self, Io, Result};
use crate::store::Account;
use lettre::address::Envelope;
use lettre::message::header::ContentType;
use lettre::message::{Mailbox, Message};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{AsyncSmtpConnection, AsyncTokioStream, TlsParameters};
use lettre::transport::smtp::extension::ClientId;
use percent_encoding::percent_decode_str;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::SystemTime;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_native_tls::{TlsConnector, native_tls};

/// 写信框里的内容；地址栏是用户输入的原文，逗号或分号分隔
#[derive(Clone, Default, PartialEq, Debug)]
pub struct Mail {
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    /// 回复时原信的 Message-ID（带尖括号）
    pub in_reply_to: Option<String>,
    /// 回复时的 References：原信的 References 再加上它的 Message-ID
    pub references: Vec<String>,
}

/// 拼好的一封
pub struct Built {
    /// 发出去的，不带 Bcc 头
    pub send: Vec<u8>,
    /// 存进已发送 / 草稿的，带 Bcc 头，看得到密送了谁
    pub copy: Vec<u8>,
    /// 发信人和全部收件人；没有收件人时为 None，只能存草稿
    pub envelope: Option<Envelope>,
}

/// 拆开逗号或分号分隔的地址，引号里的不算；空项跳过。有误时返回那一项
pub fn addresses(s: &str) -> std::result::Result<Vec<Mailbox>, String> {
    let mut quoted = false;
    s.split(|c| {
        quoted ^= c == '"';
        !quoted && (c == ',' || c == ';')
    })
    .map(str::trim)
    .filter(|a| !a.is_empty())
    .map(|a| a.parse().map_err(|_| a.to_string()))
    .collect()
}

/// 把地址拼回写信框里一栏的文字，[`addresses`] 能原样拆回
pub fn join(list: &[Mailbox]) -> String {
    list.iter().map(Mailbox::to_string).collect::<Vec<_>>().join(", ")
}

/// 以账号的名字和地址为发件人拼一封；两份的 Message-ID、Date 相同。地址有误时返回那一项
pub fn build(acc: &Account, m: &Mail) -> std::result::Result<Built, String> {
    let me = acc.email.parse().map_err(|_| acc.email.clone())?;
    let from = Mailbox::new(Some(acc.name.clone()).filter(|n| !n.is_empty()), me);
    let (to, cc, bcc) = (addresses(&m.to)?, addresses(&m.cc)?, addresses(&m.bcc)?);
    let rcpt: Vec<_> = to.iter().chain(&cc).chain(&bcc).map(|b| b.email.clone()).collect();
    let envelope = (!rcpt.is_empty()).then(|| Envelope::new(Some(from.email.clone()), rcpt)).transpose().map_err(|e| e.to_string())?;
    // 拼信时另给一个信封：没有收件人的草稿按头部推信封会出错；真正发信用上面的
    let own = Envelope::new(Some(from.email.clone()), vec![from.email.clone()]).map_err(|e| e.to_string())?;
    let id: String = std::iter::repeat_with(fastrand::alphanumeric).take(32).collect();
    let id = format!("<{id}@{}>", from.email.domain());
    let date = SystemTime::now();
    let make = |keep_bcc: bool| {
        let mut b = Message::builder().envelope(own.clone()).from(from.clone()).subject(m.subject.as_str()).message_id(Some(id.clone())).date(date);
        for x in &to {
            b = b.to(x.clone());
        }
        for x in &cc {
            b = b.cc(x.clone());
        }
        for x in &bcc {
            b = b.bcc(x.clone());
        }
        if let Some(r) = &m.in_reply_to {
            b = b.in_reply_to(r.clone());
        }
        if !m.references.is_empty() {
            b = b.references(m.references.join(" "));
        }
        if keep_bcc {
            b = b.keep_bcc();
        }
        b.header(ContentType::TEXT_PLAIN).body(m.body.clone()).map(|msg| msg.formatted()).map_err(|e| e.to_string())
    };
    Ok(Built { send: make(false)?, copy: make(true)?, envelope })
}

/// 经代理连发信服务器，加密后登录发出。587 端口先明文连上再 STARTTLS，服务器不支持就报错不发，
/// 密码、令牌不走明文；其他端口一连上就 TLS
pub async fn send(acc: &Account, secret: &Secret, b: &Built) -> Result<()> {
    let pass = secret.get(&acc.email, acc.proxy.as_deref()).await?;
    let tcp = imap::tcp(acc.proxy.as_deref(), &acc.smtp_host, acc.smtp_port).await?;
    let mut c = if acc.smtp_port == 587 {
        let mut c = connect(tcp).await?;
        c.starttls(TlsParameters::new(acc.smtp_host.clone())?, &HELLO).await?;
        c
    } else {
        connect(TlsConnector::from(native_tls::TlsConnector::new()?).connect(&acc.smtp_host, tcp).await?).await?
    };
    deliver(&mut c, &acc.email, &pass, secret.is_oauth(), b).await
}

/// EHLO 报 `[127.0.0.1]`，不报主机名
const HELLO: ClientId = ClientId::Ipv4(Ipv4Addr::LOCALHOST);

/// 在连好的流上开始 SMTP 会话；测试可以用明文 TCP 连本地测试服务器
pub async fn connect<T: Io + Sync + 'static>(stream: T) -> Result<AsyncSmtpConnection> {
    Ok(AsyncSmtpConnection::connect_with_transport(Box::new(Wire(stream)), &HELLO).await?)
}

/// 登录发出：`oauth` 时 `secret` 是访问令牌，用 XOAUTH2
pub async fn deliver(c: &mut AsyncSmtpConnection, user: &str, secret: &str, oauth: bool, b: &Built) -> Result<()> {
    let envelope = b.envelope.as_ref().ok_or("no recipients")?;
    let mechanisms: &[_] = if oauth { &[Mechanism::Xoauth2] } else { &[Mechanism::Plain, Mechanism::Login] };
    c.auth(mechanisms, &Credentials::new(user.into(), secret.into())).await?;
    c.send(envelope, &b.send).await?;
    let _ = c.quit().await;
    Ok(())
}

/// 手填服务器的邮箱，发信服务器留空时按收信服务器推：`imap.` 开头换成 `smtp.`，否则同一个主机
pub fn default_host(imap_host: &str) -> String {
    imap_host.strip_prefix("imap.").map_or_else(|| imap_host.to_string(), |h| format!("smtp.{h}"))
}

/// Gmail 发出的信自己会存进已发送，不用再存一份
pub fn saves_sent(acc: &Account) -> bool {
    acc.smtp_host.eq_ignore_ascii_case("smtp.gmail.com")
}

/// mailto: 链接（RFC 6068）里的收件人、抄送、密送、主题、正文；`+` 按字面
pub fn mailto(raw: &str) -> Option<Mail> {
    let rest = raw.get(..7).filter(|s| s.eq_ignore_ascii_case("mailto:")).map(|_| &raw[7..])?;
    let decode = |s: &str| percent_decode_str(s).decode_utf8_lossy().into_owned();
    let (to, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut m = Mail { to: decode(to), ..Default::default() };
    for (k, v) in query.split('&').filter_map(|p| p.split_once('=')) {
        let v = decode(v);
        match k.to_ascii_lowercase().as_str() {
            "to" if m.to.is_empty() => m.to = v,
            "to" => m.to = format!("{}, {v}", m.to),
            "cc" => m.cc = v,
            "bcc" => m.bcc = v,
            "subject" => m.subject = v,
            "body" => m.body = v.replace("\r\n", "\n"),
            _ => {}
        }
    }
    Some(m)
}

/// 把连好的流交给 lettre：它只收实现了 [`AsyncTokioStream`] 的流
#[derive(Debug)]
struct Wire<T>(T);

impl<T: Io> AsyncRead for Wire<T> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context, buf: &mut ReadBuf) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<T: Io> AsyncWrite for Wire<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl<T: Io + Sync> AsyncTokioStream for Wire<T> {
    /// 经代理时对方地址是代理的；lettre 发信用不到
    fn peer_addr(&self) -> std::io::Result<SocketAddr> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_sanitized_headers() {
        let acc = Account { email: "me@example.com".into(), name: "Me".into(), ..Default::default() };
        let m = Mail {
            to: "\"Li, Si\" <li@example.org>; b@example.org,".into(),
            bcc: "c@example.org".into(),
            subject: "周报".into(),
            body: "hi\nthere".into(),
            in_reply_to: Some("<x@y>".into()),
            references: vec!["<w@y>".into(), "<x@y>".into()],
            ..Default::default()
        };
        let b = build(&acc, &m).unwrap();
        assert_eq!(b.envelope.unwrap().to().len(), 3);
        let (send, copy) = (String::from_utf8(b.send).unwrap(), String::from_utf8(b.copy).unwrap());
        let head = |s: &str, name: &str| s.lines().find_map(|l| l.strip_prefix(name)).map(str::to_string);
        assert!(head(&send, "Date: ").unwrap().ends_with("+0000"), "{send}");
        assert!(head(&send, "Message-ID: ").unwrap().ends_with("@example.com>"), "{send}");
        assert_eq!(head(&send, "Message-ID: "), head(&copy, "Message-ID: "));
        assert_eq!(head(&send, "From: ").as_deref(), Some("Me <me@example.com>"));
        assert_eq!(head(&send, "In-Reply-To: ").as_deref(), Some("<x@y>"));
        assert_eq!(head(&send, "References: ").as_deref(), Some("<w@y> <x@y>"));
        assert!(head(&send, "Bcc: ").is_none() && head(&copy, "Bcc: ").is_some());
        let lower = send.to_ascii_lowercase();
        assert!(!lower.contains("user-agent") && !lower.contains("x-mailer"));

        assert_eq!(addresses("a@x.com, nope"), Err("nope".into()));
        let list = addresses("\"Li, Si\" <li@example.org>, 王五 <w@example.org>, b@example.org").unwrap();
        assert_eq!(addresses(&join(&list)).unwrap(), list);
        // 没有收件人也能拼，只是不能发
        assert!(build(&acc, &Mail::default()).unwrap().envelope.is_none());
    }

    #[test]
    fn parses_mailto() {
        let m = mailto("mailto:a+tag@x.com?subject=Hi%20there&body=1+1%0D%0A2&cc=c@x.com&to=b@x.com").unwrap();
        assert_eq!((m.to.as_str(), m.cc.as_str(), m.subject.as_str(), m.body.as_str()), ("a+tag@x.com, b@x.com", "c@x.com", "Hi there", "1+1\n2"));
        assert!(mailto("https://x.com").is_none());
        assert_eq!(default_host("imap.gmail.com"), "smtp.gmail.com");
        assert_eq!(default_host("mail.x.com"), "mail.x.com");
    }
}
