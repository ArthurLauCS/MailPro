//! 邮件 HTML 清洗：白名单标签和属性，去掉一切会联网的地址。
//!
//! 链接、内嵌图片、远程图片（含 CSS 里的 `url()`）都换成本地编号地址（`{BASE}link/n`、
//! `{BASE}cid/n`、`{BASE}img/n`），原值留在 [`Clean`] 里，查看器按编号取回。
//! 远程地址一个都不留在 HTML 里，查看器内核连域名都见不到，也就不会预解析、预连接。

use regex::{Captures, Regex};
use std::collections::HashSet;
use std::sync::LazyLock;
use std::sync::{Arc, Mutex};

/// 查看器的本地地址前缀；`.localhost` 由浏览器内核直接解析到本机，不走 DNS
pub const BASE: &str = "https://mailpro.localhost/";

#[derive(Debug, Default)]
pub struct Clean {
    pub html: String,
    /// 链接原地址，下标即 `link/n` 的 n
    pub links: Vec<String>,
    /// 内嵌图片的 Content-ID，下标即 `cid/n` 的 n
    pub cids: Vec<String>,
    /// 远程图片原地址，下标即 `img/n` 的 n；允许加载时由程序代取
    pub images: Vec<String>,
}

/// CSS 里的 `url(...)`，三种写法：双引号、单引号、不带引号
static CSS_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)url\(\s*(?:"([^"]*)"|'([^']*)'|([^)'"\s]*))\s*\)"#).unwrap());
static STYLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)(<style[^>]*>)(.*?)(</style>)").unwrap());

impl Clean {
    /// 图片地址换成本地地址；不是内嵌、data 图片或远程地址的返回 None
    fn image(&mut self, value: &str) -> Option<String> {
        if let Some(id) = value.strip_prefix("cid:") {
            self.cids.push(id.to_string());
            Some(format!("{BASE}cid/{}", self.cids.len() - 1))
        } else if value.starts_with("data:image/") {
            Some(value.into())
        } else if let Some(rest) = value.strip_prefix("//") {
            self.images.push(format!("https://{rest}"));
            Some(format!("{BASE}img/{}", self.images.len() - 1))
        } else if value.starts_with("http://") || value.starts_with("https://") {
            self.images.push(value.into());
            Some(format!("{BASE}img/{}", self.images.len() - 1))
        } else {
            None
        }
    }

    /// CSS 里的图片地址同样换成本地地址，换不了的清空
    fn css(&mut self, css: &str) -> String {
        CSS_URL
            .replace_all(css, |c: &Captures| {
                let v = c.get(1).or(c.get(2)).or(c.get(3)).map_or("", |m| m.as_str()).trim();
                format!("url(\"{}\")", self.image(v).unwrap_or_default())
            })
            .into_owned()
    }
}

pub fn clean(html: &str) -> Clean {
    let out = Arc::new(Mutex::new(Clean::default()));
    let state = out.clone();
    let html = ammonia::Builder::default()
        .add_tags(["font", "style"])
        .rm_clean_content_tags(["style"])
        // 解析器把 noscript 里的标签当文本，留着会把源码显示出来
        .add_clean_content_tags(["noscript"])
        .add_generic_attributes([
            "style", "class", "align", "valign", "width", "height", "bgcolor", "color", "border",
            "cellpadding", "cellspacing", "dir", "background",
        ])
        .add_tag_attributes("font", ["face", "size"])
        // 远程地址（含 `//host/x` 这种相对写法）先放过 scheme 检查，到下面的过滤里再删并计数
        .url_schemes(HashSet::from(["http", "https", "mailto", "cid", "data"]))
        .url_relative(ammonia::UrlRelative::PassThrough)
        .attribute_filter(move |el, attr, value| {
            let mut s = state.lock().unwrap();
            match (el, attr) {
                ("a" | "area", "href") => {
                    s.links.push(value.to_string());
                    Some(format!("{BASE}link/{}", s.links.len() - 1).into())
                }
                (_, "src" | "background") => s.image(value).map(Into::into),
                (_, "style") => Some(s.css(value).into()),
                _ => Some(value.into()),
            }
        })
        .clean(html)
        .to_string();
    let mut out = std::mem::take(&mut *out.lock().unwrap());
    // ammonia 不过问 <style> 里的内容，清洗后再改；输出是规整的 HTML，<style> 内不会出现 </style>
    out.html = STYLE.replace_all(&html, |c: &Captures| format!("{}{}{}", &c[1], out.css(&c[2]), &c[3])).into_owned();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_network_and_rewrites_local() {
        let c = clean(concat!(
            r#"<style>.a{color:red;background:url('https://t.example/s.png')}.b{background:URL(cid:bg@x)}</style><script>alert(1)</script>"#,
            r#"<div style="background-image: url( //t.example/d.png )"></div><p style="list-style:url(x.png)"></p>"#,
            r#"<img src="https://t.example/p.gif"><img src="//t.example/q.gif">"#,
            r#"<img src="cid:logo@x"><img src="data:image/png;base64,AAAA">"#,
            r#"<table><tr><td background="http://t.example/bg.png">x</td></tr></table>"#,
            r#"<a href="https://shop.example/?utm_source=m" target="_blank" onclick="x()">buy</a>"#,
            r#"<iframe src="https://t.example/f"></iframe><link rel="dns-prefetch" href="//t.example">"#,
            r#"<meta http-equiv="refresh" content="0;url=https://t.example"><base href="https://t.example/">"#,
            r#"<form action="https://t.example/f"><input></form><object data="https://t.example/o"></object>"#,
            r#"<noscript><img src="https://t.example/n.gif"></noscript>"#,
        ));
        assert_eq!(c.links, ["https://shop.example/?utm_source=m"]);
        // <style> 最后才改，排在属性里的地址后面
        assert_eq!(c.cids, ["logo@x", "bg@x"]);
        assert_eq!(c.images.len(), 6, "{:?}", c.images);
        for (i, u) in ["https://t.example/d.png", "https://t.example/p.gif", "https://t.example/q.gif", "http://t.example/bg.png"].iter().enumerate() {
            assert_eq!(&c.images[i], u);
            assert!(c.html.contains(&format!("{BASE}img/{i}")));
        }
        assert_eq!(c.images[5], "https://t.example/s.png");
        assert!(c.html.contains(&format!(r#"url("{BASE}img/5")"#)) && c.html.contains(&format!(r#"url("{BASE}cid/1")"#)));
        assert!(c.html.contains(r#"url(&quot;&quot;)"#));
        assert!(c.html.contains(&format!(r#"src="{BASE}cid/0""#)));
        assert!(c.html.contains(&format!(r#"href="{BASE}link/0""#)));
        assert!(c.html.contains("data:image/png;base64,AAAA"));
        assert!(c.html.contains(".a{color:red;"));
        for bad in ["t.example", "shop.example", "<script", "onclick", "target=", "<iframe", "<link", "<meta", "<base", "<form", "<object"] {
            assert!(!c.html.contains(bad), "{bad} survived: {}", c.html);
        }
    }

    #[test]
    fn drops_script_urls() {
        let c = clean(r#"<a href="javascript:alert(1)">x</a><img src="javascript:x">"#);
        assert!(c.links.is_empty() && !c.html.contains("javascript"));
    }
}
