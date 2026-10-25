//! 把一封原始邮件（.eml / IMAP 取回的 RFC 822 字节）变成查看器要显示的内容。

use crate::sanitize::{self, Clean};
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
}
