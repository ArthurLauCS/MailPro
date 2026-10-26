//! 把一封原始邮件（.eml / IMAP 取回的 RFC 822 字节）变成查看器要显示的内容。

use crate::sanitize::{self, Clean};
use lettre::message::Mailbox;
use mail_parser::{MessageParser, PartKind};
use regex::{Captures, Regex};
use std::sync::LazyLock;

/// 纯文本里的网址。`text_to_html` 已把 `<>&` 转义，网址里的 `&` 成了 `&amp;`，遇到 `&lt;` `&gt;` 即止
static URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"https?://(?:[^\s<>"&]|&amp;)+"#).unwrap());

pub struct Message {
    pub subject: String,
    pub from: String,
    /// 清洗后的正文；纯文本邮件由 mail-parser 转成 HTML、网址补成链接后同样清洗
    pub body: Clean,
    /// 按 `body.cids` 下标对应的内嵌资源：(Content-Type, 内容)
    pub inline: Vec<Option<(String, Vec<u8>)>>,
}

pub fn parse(raw: &[u8]) -> Option<Message> {
    let msg = MessageParser::new().parse(raw)?;
    let from = msg.from().and_then(|l| l.first()).map(|m| match (m.name(), m.address()) {
        (Some(n), Some(a)) => format!("{n} <{a}>"),
        (n, a) => n.or(a).unwrap_or_default().to_string(),
    });
    let mut html = msg.body_html(0).unwrap_or_default().into_owned();
    if msg.html_body().next().is_some_and(|p| matches!(p.kind(), PartKind::Text)) {
        html = linkify(&html);
    }
    let body = sanitize::clean(&html);
    let inline = body
        .cids
        .iter()
        .map(|cid| {
            let part = msg.parts().find(|p| p.content_id().map(|id| id.trim_matches(['<', '>'])) == Some(cid))?;
            let ct = part.content_type().map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or("octet-stream")));
            Some((ct.unwrap_or_else(|| "application/octet-stream".into()), part.decoded().into_owned()))
        })
        .collect();
    Some(Message {
        subject: msg.subject().unwrap_or_default().to_string(),
        from: from.unwrap_or_default(),
        body,
        inline,
    })
}

/// 回复、转发、接着写草稿要用的原信内容
pub struct Original {
    /// 都带尖括号
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    /// Date 头，Unix 秒
    pub date: Option<i64>,
    pub subject: String,
    pub from: Vec<Mailbox>,
    pub reply_to: Vec<Mailbox>,
    pub to: Vec<Mailbox>,
    pub cc: Vec<Mailbox>,
    pub bcc: Vec<Mailbox>,
    /// 纯文本正文，换行为 `\n`；只有 HTML 时由 mail-parser 转成文本
    pub text: String,
}

pub fn original(raw: &[u8]) -> Option<Original> {
    let msg = MessageParser::new().parse(raw)?;
    let boxes = |l: Option<mail_parser::AddressList>| -> Vec<Mailbox> {
        l.map(|l| l.mailboxes().filter_map(|m| Some(Mailbox::new(m.name().map(Into::into), m.address()?.parse().ok()?))).collect()).unwrap_or_default()
    };
    let ids = |l: Option<mail_parser::TextList>| -> Vec<String> { l.map(|l| l.iter().map(|id| format!("<{id}>")).collect()).unwrap_or_default() };
    Some(Original {
        message_id: msg.message_id().map(|id| format!("<{id}>")),
        in_reply_to: ids(msg.in_reply_to()).pop(),
        references: ids(msg.references()),
        date: msg.date().map(|d| d.to_timestamp()),
        subject: msg.subject().unwrap_or_default().to_string(),
        from: boxes(msg.from()),
        reply_to: boxes(msg.reply_to()),
        to: boxes(msg.to()),
        cc: boxes(msg.cc()),
        bcc: boxes(msg.bcc()),
        text: msg.body_text(0).unwrap_or_default().replace("\r\n", "\n").trim_end().to_string(),
    })
}

impl Original {
    /// 回复的收件人和抄送：回给 Reply-To，没有就回发件人；全部回复再加上原收件人、抄送。
    /// 都去掉自己（`me`）和重复的；回的是自己发的信时改回给原收件人
    pub fn recipients(&self, all: bool, me: &str) -> (Vec<Mailbox>, Vec<Mailbox>) {
        let mut seen = vec![me.to_ascii_lowercase()];
        let mut keep = |list: &[Mailbox]| {
            let mut out = Vec::new();
            for m in list {
                let addr = m.email.to_string().to_ascii_lowercase();
                if !seen.contains(&addr) {
                    seen.push(addr);
                    out.push(m.clone());
                }
            }
            out
        };
        let mut to = keep(if self.reply_to.is_empty() { &self.from } else { &self.reply_to });
        if all || to.is_empty() {
            to.extend(keep(&self.to));
        }
        let cc = if all { keep(&self.cc) } else { Vec::new() };
        (to, cc)
    }

    /// 回复时的 In-Reply-To 和 References
    pub fn thread(&self) -> (Option<String>, Vec<String>) {
        let mut refs = self.references.clone();
        refs.extend(self.message_id.clone().filter(|id| !refs.contains(id)));
        (self.message_id.clone(), refs)
    }
}

/// 回复的主题：前加 `Re: `，已经有了就不加（不分大小写）
pub fn reply_subject(s: &str) -> String {
    prefixed(s, "Re: ", &["re:"])
}

/// 转发的主题：前加 `Fwd: `，已有 `Fwd:` / `Fw:` 就不加
pub fn forward_subject(s: &str) -> String {
    prefixed(s, "Fwd: ", &["fwd:", "fw:"])
}

fn prefixed(s: &str, prefix: &str, known: &[&str]) -> String {
    let lower = s.to_lowercase();
    if known.iter().any(|k| lower.starts_with(k)) { s.to_string() } else { format!("{prefix}{s}") }
}

/// 引用原文：每行前加 `> `，空行只加 `>`
pub fn quote(text: &str) -> String {
    text.lines().map(|l| if l.is_empty() { ">".to_string() } else { format!("> {l}") }).collect::<Vec<_>>().join("\n")
}

/// 把纯文本转成的 HTML 里的网址包成 `<a>`；句末标点不算网址
fn linkify(html: &str) -> String {
    URL.replace_all(html, |c: &Captures| {
        let url = c[0].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', '\'']);
        format!(r#"<a href="{url}">{url}</a>{}"#, &c[0][url.len()..])
    })
    .into_owned()
}

#[cfg(test)]
mod tests {
    #[test]
    fn links_plain_text() {
        let m = super::parse(b"Subject: x\r\nContent-Type: text/plain\r\n\r\nSee https://a.example/t?code=1&b=2. Or <https://b.example/>\r\n").unwrap();
        assert_eq!(m.body.links, ["https://a.example/t?code=1&b=2", "https://b.example/"]);
        assert!(m.body.html.contains(r#"">https://b.example/</a>&gt;"#), "{}", m.body.html);
    }

    #[test]
    fn prepares_replies() {
        let raw = b"From: Li <li@x.com>\r\nTo: me@x.com, Wang <w@x.com>\r\nCc: ME@x.com, z@x.com, w@x.com\r\nSubject: RE: plan\r\n\
            Message-ID: <b@x.com>\r\nReferences: <a@x.com>\r\nDate: Mon, 26 Oct 2026 08:00:00 +0000\r\n\r\nline 1\r\n\r\nline 2\r\n\r\n";
        let o = super::original(raw).unwrap();
        let emails = |l: &[super::Mailbox]| l.iter().map(|m| m.email.to_string()).collect::<Vec<_>>();
        let (to, cc) = o.recipients(false, "me@x.com");
        assert_eq!((emails(&to), emails(&cc)), (vec!["li@x.com".to_string()], vec![]));
        // 全部回复：去掉自己（不分大小写）和已在收件人里的
        let (to, cc) = o.recipients(true, "me@x.com");
        assert_eq!((emails(&to), emails(&cc)), (vec!["li@x.com".to_string(), "w@x.com".into()], vec!["z@x.com".to_string()]));
        // 回自己发的信：改回给原收件人
        assert_eq!(emails(&o.recipients(false, "li@x.com").0), ["me@x.com", "w@x.com"]);
        assert_eq!(o.thread(), (Some("<b@x.com>".into()), vec!["<a@x.com>".into(), "<b@x.com>".into()]));
        assert_eq!(o.date, Some(1_793_001_600));
        assert_eq!(super::quote(&o.text), "> line 1\n>\n> line 2");
        assert_eq!((super::reply_subject(&o.subject), super::reply_subject("plan")), ("RE: plan".into(), "Re: plan".into()));
        assert_eq!((super::forward_subject("FW: plan"), super::forward_subject("plan")), ("FW: plan".into(), "Fwd: plan".into()));
    }
}
