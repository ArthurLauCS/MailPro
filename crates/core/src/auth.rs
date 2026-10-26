//! 登录凭据。密码直接用；Outlook / Hotmail 只收 OAuth2（XOAUTH2）：在浏览器里登录个人微软账号
//! （授权码 + PKCE，回调到本机回环地址），凭据管理器里存刷新令牌，访问令牌只放内存。
//! 令牌请求和 IMAP 一样经邮箱的代理（[`imap::tcp`]），代理连不上直接报错；登录页的流量归浏览器自己管。

use crate::cred;
use crate::http;
use crate::imap::{self, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::future::{self, FutureExt};
use futures::lock::Mutex;
use futures::stream::{FuturesUnordered, StreamExt};
use serde_json::Value;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::pin::pin;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::{Url, form_urlencoded};
use windows::Win32::Security::Cryptography::{BCRYPT_SHA256_ALG_HANDLE, BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom, BCryptHash};

/// Entra 里注册的 MailPro：只收个人微软账号，公共客户端（移动和桌面），回调 `http://localhost`，没有密钥
const CLIENT_ID: &str = "9d57760b-bc00-4fef-af2f-b9874c3a66c0";
/// 只收个人账号的应用用 consumers 端点
const AUTHORIZE: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/authorize";
const TOKEN: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
/// 收信、发信；有 offline_access 才给刷新令牌
const SCOPE: &str = "https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send offline_access";
/// 访问令牌离过期不到这么久就换新的，免得刚连上就过期
const MARGIN: Duration = Duration::from_secs(5 * 60);
/// 令牌请求卡住时放弃
const TIMEOUT: Duration = Duration::from_secs(30);
const DONE: &str = "<!doctype html><meta charset=utf-8><title>MailPro</title>\
    <p>已收到授权，可以关掉这个页面，回到 MailPro。<p>Authorization received. You can close this page and return to MailPro.";
const FAILED: &str = "<!doctype html><meta charset=utf-8><title>MailPro</title>\
    <p>登录没有完成，原因见 MailPro。<p>Sign-in was not completed. See MailPro for details.";

/// 一个账号的登录凭据；同一个账号的几条连接共用一份
pub enum Secret {
    Password(String),
    OAuth(Mutex<Tokens>),
}

pub struct Tokens {
    /// None：令牌端点拒了它，要重新登录
    refresh: Option<String>,
    /// 访问令牌和该换新的时刻
    access: Option<(String, Instant)>,
}

/// 令牌端点拒绝了（授权过期、被撤销、改了密码、应用配置不对）：重试没用，要重新登录
#[derive(Debug)]
pub struct Expired(pub String);

impl std::fmt::Display for Expired {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Expired {}

impl Secret {
    /// 从凭据管理器取出账号存的那份：Outlook 是刷新令牌，其他是密码
    pub fn load(email: &str) -> Result<Secret> {
        let s = cred::load(email)?;
        Ok(if imap::oauth_only(email) { Secret::OAuth(Mutex::new(Tokens { refresh: Some(s), access: None })) } else { Secret::Password(s) })
    }

    /// 存进凭据管理器：密码或刷新令牌
    pub async fn save(&self, email: &str) -> Result<()> {
        let s = match self {
            Secret::Password(p) => p.clone(),
            Secret::OAuth(t) => t.lock().await.refresh.clone().ok_or("no refresh token")?,
        };
        Ok(cred::save(email, &s)?)
    }

    pub fn is_oauth(&self) -> bool {
        matches!(self, Secret::OAuth(_))
    }

    /// 登录用的密码或访问令牌。访问令牌快过期时经 `proxy` 用刷新令牌换新的，新的刷新令牌替换凭据管理器里
    /// 旧的；同时只有一条连接在换
    pub async fn get(&self, email: &str, proxy: Option<&str>) -> Result<String> {
        let t = match self {
            Secret::Password(p) => return Ok(p.clone()),
            Secret::OAuth(t) => t,
        };
        let mut t = t.lock().await;
        if let Some((token, until)) = &t.access
            && Instant::now() < *until
        {
            return Ok(token.clone());
        }
        let refresh = t.refresh.clone().ok_or_else(|| Expired("refresh token rejected".into()))?;
        let (access, until, refresh) = match request(&[("grant_type", "refresh_token"), ("refresh_token", refresh.as_str())], proxy).await {
            // 记下来，别的连接不用再问令牌端点
            Err(e) if e.is::<Expired>() => {
                t.refresh = None;
                return Err(e);
            }
            r => r?,
        };
        if let Some(r) = refresh {
            cred::save(email, &r)?;
            t.refresh = Some(r);
        }
        t.access = Some((access.clone(), until));
        Ok(access)
    }
}

/// 在浏览器里登录 `email`：在本机回环地址上起临时监听，把授权地址交给 `open` 去开浏览器，
/// 等浏览器带着授权码回来（state 对上才算），再经 `proxy` 换令牌。
/// 不设超时：丢掉这个 future 即取消，监听随之关闭
pub async fn sign_in(email: &str, proxy: Option<&str>, open: impl FnOnce(String)) -> Result<Secret> {
    let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = v4.local_addr()?.port();
    // 浏览器可能把 localhost 解析成 ::1，同一端口在 ::1 上也听着；本机没有 IPv6 就只听 127.0.0.1
    let v6 = TcpListener::bind((Ipv6Addr::LOCALHOST, port)).await.ok();
    // 和注册的 http://localhost 一致：微软对 localhost 回调不比端口
    let redirect = format!("http://localhost:{port}");
    let (verifier, state) = (random::<32>()?, random::<16>()?);
    let challenge = URL_SAFE_NO_PAD.encode(sha256(verifier.as_bytes())?);
    let url = Url::parse_with_params(
        AUTHORIZE,
        [
            ("client_id", CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", redirect.as_str()),
            ("response_mode", "query"),
            ("scope", SCOPE),
            ("state", state.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            // 预填邮箱，并且总让用户选账号：浏览器里已登录的别的微软账号不会被直接拿来用
            ("login_hint", email),
            ("prompt", "select_account"),
        ],
    )?;
    open(url.into());
    let code = code(&v4, v6.as_ref(), &state).await?;
    let params = [("grant_type", "authorization_code"), ("code", code.as_str()), ("redirect_uri", redirect.as_str()), ("code_verifier", verifier.as_str())];
    let (access, until, refresh) = request(&params, proxy).await?;
    Ok(Secret::OAuth(Mutex::new(Tokens { refresh: Some(refresh.ok_or("no refresh token")?), access: Some((access, until)) })))
}

/// 等浏览器带着授权码回来。几条连接一起读：浏览器可能先开好连接不发请求
async fn code(v4: &TcpListener, v6: Option<&TcpListener>, state: &str) -> Result<String> {
    let mut conns = FuturesUnordered::new();
    loop {
        futures::select! {
            s = accept(v4, v6).fuse() => conns.push(callback(s?, state)),
            r = conns.select_next_some() => if let Some(r) = r { return r },
        }
    }
}

async fn accept(v4: &TcpListener, v6: Option<&TcpListener>) -> std::io::Result<TcpStream> {
    Ok(match v6 {
        Some(v6) => future::select(pin!(v4.accept()), pin!(v6.accept())).await.factor_first().0?.0,
        None => v4.accept().await?.0,
    })
}

/// 读一个回调请求，回一页结果。不是这次登录的回调（state 对不上、浏览器要图标等）回 404，返回 None
async fn callback(mut s: TcpStream, state: &str) -> Option<Result<String>> {
    // 请求头读到空行为止，限 16 KB
    let mut buf = Vec::new();
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let mut chunk = [0; 4096];
        match s.read(&mut chunk).await {
            Ok(n) if n > 0 && buf.len() < 16 << 10 => buf.extend_from_slice(&chunk[..n]),
            _ => return None,
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let target = head.strip_prefix("GET ").and_then(|l| l.split(' ').next()).unwrap_or_default();
    let url = Url::parse("http://localhost").and_then(|u| u.join(target)).ok()?;
    let q = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
    let r = match (q("state").filter(|s| s == state), q("code")) {
        (None, _) => {
            let _ = reply(&mut s, "404 Not Found", "").await;
            return None;
        }
        (Some(_), Some(code)) => Ok(code),
        // 用户在登录页取消（access_denied）或授权出错
        (Some(_), None) => Err(format!("{}: {}", q("error").unwrap_or_default(), q("error_description").unwrap_or_default()).into()),
    };
    let _ = reply(&mut s, "200 OK", if r.is_ok() { DONE } else { FAILED }).await;
    Some(r)
}

async fn reply(s: &mut TcpStream, status: &str, page: &str) -> std::io::Result<()> {
    let head = format!("HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", page.len());
    s.write_all((head + page).as_bytes()).await?;
    s.shutdown().await
}

/// 向令牌端点换令牌（授权码或刷新令牌），回 (访问令牌, 该换新的时刻, 新的刷新令牌)
async fn request(params: &[(&str, &str)], proxy: Option<&str>) -> Result<(String, Instant, Option<String>)> {
    let form = form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", CLIENT_ID)
        .append_pair("scope", SCOPE)
        .extend_pairs(params)
        .finish();
    let (status, body) = tokio::time::timeout(TIMEOUT, http::post(TOKEN, &form, proxy)).await.map_err(|_| "token request timeout")??;
    tokens(status, &body)
}

/// 解读令牌端点的回复。拒绝（4xx 带 error，忙的 temporarily_unavailable 除外）报 [`Expired`]；
/// 其余失败照常报错，可以重试
fn tokens(status: u16, body: &[u8]) -> Result<(String, Instant, Option<String>)> {
    let v: Value = serde_json::from_slice(body).unwrap_or_default();
    let s = |k: &str| v[k].as_str().map(str::to_string);
    let error = s("error").unwrap_or_default();
    match (status, s("access_token")) {
        (200, Some(access)) => {
            let life = Duration::from_secs(v["expires_in"].as_u64().unwrap_or(3600));
            Ok((access, Instant::now() + life.saturating_sub(MARGIN), s("refresh_token")))
        }
        (400..500, _) if !error.is_empty() && error != "temporarily_unavailable" => {
            // 说明后面还跟着 Trace ID 等几行，只留第一行
            let desc = s("error_description").unwrap_or_default();
            Err(Expired(format!("{error}: {}", desc.lines().next().unwrap_or_default())).into())
        }
        _ => Err(format!("token endpoint: HTTP {status} {error}").into()),
    }
}

/// 系统的密码学随机数，写成 URL 安全的 base64（PKCE 的 verifier、state）
fn random<const N: usize>() -> Result<String> {
    let mut b = [0u8; N];
    unsafe { BCryptGenRandom(None, &mut b, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }.ok()?;
    Ok(URL_SAFE_NO_PAD.encode(b))
}

fn sha256(data: &[u8]) -> Result<[u8; 32]> {
    let mut out = [0u8; 32];
    unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut out) }.ok()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce() {
        // RFC 7636 附录 B 的例子
        assert_eq!(URL_SAFE_NO_PAD.encode(sha256(b"dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk").unwrap()), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        let v = random::<32>().unwrap();
        assert_eq!(v.len(), 43);
        assert_ne!(v, random::<32>().unwrap());
    }

    #[test]
    fn reads_token_replies() {
        let (access, until, refresh) = tokens(200, br#"{"access_token":"a","refresh_token":"r","expires_in":3600}"#).unwrap();
        assert_eq!((access.as_str(), refresh.as_deref()), ("a", Some("r")));
        assert!(until > Instant::now() + Duration::from_secs(3000));
        let e = tokens(400, br#"{"error":"invalid_grant","error_description":"AADSTS70000: bad.\r\nTrace ID: x"}"#).unwrap_err();
        assert_eq!(e.downcast_ref::<Expired>().unwrap().0, "invalid_grant: AADSTS70000: bad.");
        // 忙、服务器出错：可以重试，不算授权失效
        assert!(!tokens(400, br#"{"error":"temporarily_unavailable"}"#).unwrap_err().is::<Expired>());
        assert!(!tokens(503, b"").unwrap_err().is::<Expired>());
    }

    #[test]
    fn waits_for_matching_callback() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let port = l.local_addr().unwrap().port();
            let get = move |path: &'static str| async move {
                let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await.unwrap();
                s.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n").as_bytes()).await.unwrap();
                let mut r = String::new();
                s.read_to_string(&mut r).await.unwrap();
                r
            };
            let (r, ()) = futures::join!(code(&l, None, "s1"), async {
                // 浏览器先开着一条不发请求的连接；别的路径、别的 state 都不算
                let _idle = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await.unwrap();
                assert!(get("/favicon.ico").await.starts_with("HTTP/1.1 404"));
                assert!(get("/?code=bad&state=s0").await.starts_with("HTTP/1.1 404"));
                assert!(get("/?code=c%2B1&state=s1").await.starts_with("HTTP/1.1 200"));
            });
            assert_eq!(r.unwrap(), "c+1");
            // 用户在登录页取消
            let (r, page) = futures::join!(code(&l, None, "s2"), get("/?error=access_denied&error_description=canceled&state=s2"));
            assert_eq!(r.unwrap_err().to_string(), "access_denied: canceled");
            assert!(page.contains("Sign-in was not completed"));
        });
    }
}
