//! 点链接前的清洗：拆开地址里带着目标的跳转壳，去掉跟踪参数。
//!
//! 只认地址里直接带目标的壳；Mailchimp、SendGrid 这类目标存在对方服务器上的点击追踪，
//! 不访问就不知道去哪，原样留着。

use url::Url;

pub struct Link {
    /// 清洗后的地址；不是 http(s) 或解析不了时为原样
    pub url: String,
    /// 拆掉的跳转壳的域名，由外到内
    pub unwrapped: Vec<String>,
    /// 去掉的参数名
    pub removed: Vec<String>,
}

/// (域名，路径，带目标地址的参数)；域名按后缀匹配
const WRAPPERS: &[(&str, &str, &str)] = &[
    ("google.com", "/url", "q"),
    ("safelinks.protection.outlook.com", "/", "url"),
    ("l.facebook.com", "/l.php", "u"),
    ("lm.facebook.com", "/l.php", "u"),
    ("l.instagram.com", "/", "u"),
    ("youtube.com", "/redirect", "q"),
    ("linkedin.com", "/redir/redirect", "url"),
    ("slack-redir.net", "/link", "url"),
    ("steamcommunity.com", "/linkfilter/", "url"),
];

/// 以这些开头的参数都去掉
const PREFIXES: &[&str] = &["utm_", "__hs"];

const PARAMS: &[&str] = &[
    "fbclid", "gclid", "gclsrc", "dclid", "gbraid", "wbraid", "msclkid", "yclid", "twclid", "ttclid", "igshid", "srsltid", "mc_cid",
    "mc_eid", "_hsenc", "_hsmi", "mkt_tok", "oly_anon_id", "oly_enc_id", "vero_conv", "vero_id", "rb_clickid", "s_cid", "ml_subscriber",
    "ml_subscriber_hash", "_kx", "wickedid",
];

pub fn clean(raw: &str) -> Link {
    let mut link = Link { url: raw.to_string(), unwrapped: Vec::new(), removed: Vec::new() };
    let Ok(mut u) = Url::parse(raw) else { return link };
    // 层数封顶，防止互相套的地址转不出来
    for _ in 0..8 {
        let Some(inner) = unwrap(&u) else { break };
        link.unwrapped.push(u.host_str().unwrap_or_default().to_string());
        u = inner;
    }
    if !matches!(u.scheme(), "http" | "https") {
        return link;
    }
    // 按原样的 `&` 分段过滤，不重新编码留下的参数，免得改坏对方要的写法
    if let Some(q) = u.query().map(str::to_string) {
        let keep: Vec<_> = q
            .split('&')
            .filter(|seg| {
                let key = seg.split('=').next().unwrap_or_default();
                let drop = PREFIXES.iter().any(|p| key.starts_with(p)) || PARAMS.contains(&key);
                if drop {
                    link.removed.push(key.to_string());
                }
                !drop
            })
            .collect();
        u.set_query(Some(&keep.join("&")).filter(|q| !q.is_empty()).map(String::as_str));
    }
    link.url = u.into();
    link
}

/// 能交给浏览器的 http(s) 地址，按标准格式重新写出（空格、引号等都已转义）
pub fn web(raw: &str) -> Option<String> {
    Url::parse(raw).ok().filter(|u| matches!(u.scheme(), "http" | "https")).map(String::from)
}

pub fn host(raw: &str) -> Option<String> {
    Url::parse(raw).ok()?.host_str().map(str::to_string)
}

fn unwrap(u: &Url) -> Option<Url> {
    let host = u.host_str()?;
    let (_, _, param) = WRAPPERS
        .iter()
        .find(|(h, path, _)| (host == *h || host.ends_with(&format!(".{h}"))) && u.path() == *path)?;
    let target = u.query_pairs().find(|(k, _)| k == param)?.1;
    Url::parse(&target).ok().filter(|t| matches!(t.scheme(), "http" | "https"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_and_strips() {
        let l = clean("https://www.google.com/url?sa=t&q=https%3A%2F%2Fnam12.safelinks.protection.outlook.com%2F%3Furl%3Dhttps%253A%252F%252Fshop.example%252Fa%253Fid%253D7%2526utm_source%253Dm%2526fbclid%253Dx%26data%3D1");
        assert_eq!(l.url, "https://shop.example/a?id=7");
        assert_eq!(l.unwrapped, ["www.google.com", "nam12.safelinks.protection.outlook.com"]);
        assert_eq!(l.removed, ["utm_source", "fbclid"]);
    }

    #[test]
    fn keeps_encoding_and_fragment() {
        let l = clean("https://a.example/p?q=a%20b+c&utm_medium=x&_hsenc=y#top");
        assert_eq!(l.url, "https://a.example/p?q=a%20b+c#top");
        let l = clean("https://a.example/p?utm_campaign=z");
        assert_eq!((l.url.as_str(), l.removed.len()), ("https://a.example/p", 1));
    }

    #[test]
    fn leaves_others_alone() {
        for raw in ["https://click.list-manage.com/track/click?u=1&id=2", "mailto:a@b.example?subject=utm_x", "/relative?utm_x=1"] {
            let l = clean(raw);
            assert!(l.url == raw && l.unwrapped.is_empty() && l.removed.is_empty(), "{raw} -> {}", l.url);
        }
        // 壳里的目标不是网页地址就不拆
        assert!(clean("https://www.google.com/url?q=javascript:alert(1)").unwrapped.is_empty());
    }
}
