//! 代取允许加载的远程图片：和 IMAP 一样经邮箱的代理（[`imap::tcp`]），不带 cookie、Referer，
//! User-Agent 用通用值。只做最简单的 HTTP/1.1 GET：每次新连接，读到对方断开，跟随跳转。

use crate::imap::{self, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_native_tls::{TlsConnector, native_tls};
use url::{Position, Url};

/// 单张图片的上限，超过的截断后按不完整报错
const MAX: u64 = 20 << 20;
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";

enum Reply {
    Moved(String),
    /// (Content-Type, 内容)
    Done(String, Vec<u8>),
}

/// 返回 (Content-Type, 内容)；不是图片类型的一律报成 `application/octet-stream`
pub async fn get(url: &str, proxy: Option<&str>) -> Result<(String, Vec<u8>)> {
    let mut url = Url::parse(url)?;
    for _ in 0..5 {
        let host = url.host_str().ok_or("no host")?.to_string();
        let port = url.port_or_known_default().ok_or("no port")?;
        let req = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {UA}\r\nAccept: image/*\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n",
            &url[Position::BeforePath..Position::AfterQuery],
            &url[Position::BeforeHost..Position::AfterPort],
        );
        let tcp = imap::tcp(proxy, &host, port).await?;
        let raw = match url.scheme() {
            "https" => exchange(TlsConnector::from(native_tls::TlsConnector::new()?).connect(&host, tcp).await?, &req).await?,
            "http" => exchange(tcp, &req).await?,
            s => return Err(format!("scheme {s}").into()),
        };
        match parse(&raw)? {
            Reply::Moved(to) => url = url.join(&to)?,
            Reply::Done(ct, body) if ct.starts_with("image/") => return Ok((ct, body)),
            Reply::Done(_, body) => return Ok(("application/octet-stream".into(), body)),
        }
    }
    Err("too many redirects".into())
}

async fn exchange(mut s: impl AsyncRead + AsyncWrite + Unpin, req: &str) -> Result<Vec<u8>> {
    s.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    // 有的服务器不发 TLS 关闭通知就断开；已到手的照用，完不完整由 parse 按长度判断
    if let Err(e) = (&mut s).take(MAX).read_to_end(&mut buf).await {
        if buf.is_empty() {
            return Err(e.into());
        }
    }
    Ok(buf)
}

fn parse(raw: &[u8]) -> Result<Reply> {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or("incomplete header")?;
    let head = String::from_utf8_lossy(&raw[..end]);
    let status: u16 = head.split(' ').nth(1).and_then(|s| s.parse().ok()).ok_or("bad status line")?;
    let header = |name: &str| {
        head.split("\r\n").skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    };
    let body = &raw[end + 4..];
    if (300..400).contains(&status) {
        if let Some(to) = header("location") {
            return Ok(Reply::Moved(to));
        }
    }
    if status != 200 {
        return Err(format!("HTTP {status}").into());
    }
    let body = if header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        unchunk(body).ok_or("incomplete body")?
    } else if let Some(n) = header("content-length").and_then(|v| v.parse().ok()) {
        body.get(..n).ok_or("incomplete body")?.to_vec()
    } else {
        body.to_vec()
    };
    let ct = header("content-type").map(|v| v.split(';').next().unwrap_or_default().trim().to_ascii_lowercase());
    Ok(Reply::Done(ct.unwrap_or_default(), body))
}

fn unchunk(mut b: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let i = b.windows(2).position(|w| w == b"\r\n")?;
        let n = usize::from_str_radix(std::str::from_utf8(&b[..i]).ok()?.split(';').next()?.trim(), 16).ok()?;
        if n == 0 {
            return Some(out);
        }
        out.extend_from_slice(b.get(i + 2..i + 2 + n)?);
        b = b.get(i + 4 + n..)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_replies() {
        let r = parse(b"HTTP/1.1 302 Found\r\nlocation: /p.gif\r\n\r\n").unwrap();
        assert!(matches!(r, Reply::Moved(to) if to == "/p.gif"));
        let r = parse(b"HTTP/1.1 200 OK\r\nContent-Type: Image/GIF; x=1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nGIF\r\n2;e\r\n89\r\n0\r\n\r\n").unwrap();
        assert!(matches!(r, Reply::Done(ct, b) if ct == "image/gif" && b == b"GIF89"));
        let r = parse(b"HTTP/1.0 200 OK\r\nContent-Length: 3\r\n\r\nabcdef").unwrap();
        assert!(matches!(r, Reply::Done(ct, b) if ct.is_empty() && b == b"abc"));
        // 截断的不能当成完整图片
        assert!(parse(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nabc").is_err());
        assert!(parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n9\r\nabc").is_err());
        assert!(parse(b"HTTP/1.1 404 Not Found\r\n\r\n").is_err());
    }
}
