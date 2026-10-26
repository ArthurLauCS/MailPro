//! 本地缓存：账号、文件夹、邮件头和标记、下载过的正文，以及还没写回服务器的已读改动。

use rusqlite::{Connection, OptionalExtension, Result, params};
use std::collections::HashSet;
use std::time::Duration;

/// 标记位，存在 `message.flags`
pub const SEEN: u8 = 1;
pub const FLAGGED: u8 = 2;
pub const ANSWERED: u8 = 4;

#[derive(Clone, Default)]
pub struct Account {
    pub id: i64,
    /// 同时是登录名
    pub email: String,
    pub host: String,
    pub port: u16,
    /// SOCKS5 代理 `主机:端口`；None 为直连
    pub proxy: Option<String>,
    /// 发信时的显示名，空为只写地址
    pub name: String,
    /// 发信服务器，只走隐式 TLS（通常 465 端口）
    pub smtp_host: String,
    pub smtp_port: u16,
}

#[derive(Clone)]
pub struct Folder {
    pub id: i64,
    /// 服务器上的原名，非 ASCII 是 modified UTF-7；显示用 [`crate::imap::folder_name`]
    pub name: String,
    pub uidvalidity: Option<u32>,
    /// 服务器的 HIGHESTMODSEQ，整个文件夹同步完才记下
    pub modseq: Option<u64>,
    /// 首次全量同步已完成；未完成时每次同步都和服务器对账，断了接着补
    pub complete: bool,
    /// 特殊用途：inbox、flagged、important、sent、drafts、all、archive、junk、trash；自建文件夹为 None
    pub role: Option<String>,
}

pub struct Header {
    /// 所在文件夹；[`Store::put_headers`] 不看它，按参数存
    pub folder: i64,
    pub uid: u32,
    pub flags: u8,
    /// 服务器收到的时间（INTERNALDATE），Unix 秒
    pub date: i64,
    pub size: u32,
    pub attach: bool,
    pub from_name: String,
    pub from_addr: String,
    pub subject: String,
}

const SCHEMA: &str = "
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS account (
    id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE, host TEXT NOT NULL, port INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS folder (
    id INTEGER PRIMARY KEY, account INTEGER NOT NULL REFERENCES account ON DELETE CASCADE,
    name TEXT NOT NULL, uidvalidity INTEGER, modseq INTEGER, complete INTEGER NOT NULL DEFAULT 0,
    UNIQUE (account, name));
CREATE TABLE IF NOT EXISTS message (
    folder INTEGER NOT NULL REFERENCES folder ON DELETE CASCADE, uid INTEGER NOT NULL,
    flags INTEGER NOT NULL, date INTEGER NOT NULL, size INTEGER NOT NULL, attach INTEGER NOT NULL,
    from_name TEXT NOT NULL, from_addr TEXT NOT NULL, subject TEXT NOT NULL,
    PRIMARY KEY (folder, uid)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS message_date ON message (folder, date);
-- 原始 RFC 822 字节，下载后一直留着
CREATE TABLE IF NOT EXISTS body (
    folder INTEGER NOT NULL, uid INTEGER NOT NULL, raw BLOB NOT NULL, PRIMARY KEY (folder, uid),
    FOREIGN KEY (folder, uid) REFERENCES message ON DELETE CASCADE);
-- 本地改了、还没写回服务器的标记：mask 是改过的标记位，bits 是它们的新值（第 3 条迁移把 seen 改名为 bits、加 mask）；
-- 邮件在服务器上删掉时跟着删
CREATE TABLE IF NOT EXISTS pending (
    folder INTEGER NOT NULL, uid INTEGER NOT NULL, seen INTEGER NOT NULL, PRIMARY KEY (folder, uid),
    FOREIGN KEY (folder, uid) REFERENCES message ON DELETE CASCADE);
-- 本软件自己的设置，如界面语言
CREATE TABLE IF NOT EXISTS setting (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- 总是加载远程图片的发件人，地址存小写
CREATE TABLE IF NOT EXISTS image_sender (addr TEXT PRIMARY KEY) WITHOUT ROWID;
";

/// 建表之后按顺序执行，`PRAGMA user_version` 记已执行的条数；只能往后加
const MIGRATIONS: &[&str] = &[
    "ALTER TABLE account ADD COLUMN proxy TEXT",
    "ALTER TABLE folder ADD COLUMN role TEXT",
    // 原有的行只记已读：seen 的 0 / 1 正好是 SEEN 位的值
    "ALTER TABLE pending RENAME COLUMN seen TO bits; ALTER TABLE pending ADD COLUMN mask INTEGER NOT NULL DEFAULT 1",
    // 已有账号的发信服务器按 [`crate::smtp::default_host`] 的规则从收信服务器推出
    "ALTER TABLE account ADD COLUMN name TEXT NOT NULL DEFAULT '';
     ALTER TABLE account ADD COLUMN smtp_host TEXT NOT NULL DEFAULT '';
     ALTER TABLE account ADD COLUMN smtp_port INTEGER NOT NULL DEFAULT 465;
     UPDATE account SET smtp_host = CASE WHEN host LIKE 'imap.%' THEN 'smtp.' || substr(host, 6) ELSE host END",
];

pub struct Store(Connection);

/// 读 [`Header`] 的列，和 [`header`] 的下标对应
const HEADER: &str = "folder, uid, flags, date, size, attach, from_name, from_addr, subject";

fn header(r: &rusqlite::Row) -> Result<Header> {
    Ok(Header {
        folder: r.get(0)?,
        uid: r.get(1)?,
        flags: r.get(2)?,
        date: r.get(3)?,
        size: r.get(4)?,
        attach: r.get(5)?,
        from_name: r.get(6)?,
        from_addr: r.get(7)?,
        subject: r.get(8)?,
    })
}

/// `%LOCALAPPDATA%\MailPro\mail.db`，目录不存在就建
pub fn path() -> std::io::Result<String> {
    let dir = std::env::var("LOCALAPPDATA").map_err(std::io::Error::other)? + "\\MailPro";
    std::fs::create_dir_all(&dir)?;
    Ok(dir + "\\mail.db")
}

impl Store {
    pub fn open(path: &str) -> Result<Store> {
        let db = Connection::open(path)?;
        // 命令行和后台同步可能同时写同一个库
        db.busy_timeout(Duration::from_secs(5))?;
        db.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        db.execute_batch(SCHEMA)?;
        let done: usize = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, m) in MIGRATIONS.iter().enumerate().skip(done) {
            db.execute_batch(&format!("BEGIN; {m}; PRAGMA user_version = {}; COMMIT;", i + 1))?;
        }
        Ok(Store(db))
    }

    /// 新增或按邮箱更新，返回账号 id
    pub fn add_account(&self, a: &Account) -> Result<i64> {
        self.0.query_row(
            "INSERT INTO account (email, host, port, proxy, name, smtp_host, smtp_port) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (email) DO UPDATE SET host = ?2, port = ?3, proxy = ?4, name = ?5, smtp_host = ?6, smtp_port = ?7 RETURNING id",
            params![a.email, a.host, a.port, a.proxy, a.name, a.smtp_host, a.smtp_port],
            |r| r.get(0),
        )
    }

    pub fn set_proxy(&self, account: i64, proxy: Option<&str>) -> Result<()> {
        self.0.execute("UPDATE account SET proxy = ?2 WHERE id = ?1", params![account, proxy])?;
        Ok(())
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        self.0.query_row("SELECT value FROM setting WHERE key = ?1", [key], |r| r.get(0)).optional()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.0.execute("INSERT OR REPLACE INTO setting (key, value) VALUES (?1, ?2)", [key, value])?;
        Ok(())
    }

    /// 发件人在不在“总是加载图片”名单里；地址不分大小写
    pub fn images_allowed(&self, addr: &str) -> Result<bool> {
        self.0.query_row("SELECT 1 FROM image_sender WHERE addr = ?1", [addr.to_lowercase()], |_| Ok(())).optional().map(|r| r.is_some())
    }

    pub fn allow_images(&self, addr: &str, allow: bool) -> Result<()> {
        let sql = if allow { "INSERT OR IGNORE INTO image_sender (addr) VALUES (?1)" } else { "DELETE FROM image_sender WHERE addr = ?1" };
        self.0.execute(sql, [addr.to_lowercase()])?;
        Ok(())
    }

    pub fn accounts(&self) -> Result<Vec<Account>> {
        let mut st = self.0.prepare("SELECT id, email, host, port, proxy, name, smtp_host, smtp_port FROM account ORDER BY id")?;
        st.query_map([], |r| {
            Ok(Account {
                id: r.get(0)?,
                email: r.get(1)?,
                host: r.get(2)?,
                port: r.get(3)?,
                proxy: r.get(4)?,
                name: r.get(5)?,
                smtp_host: r.get(6)?,
                smtp_port: r.get(7)?,
            })
        })?
        .collect()
    }

    /// 按服务器的文件夹列表（原名, 特殊用途）增删本地文件夹，删掉的连同邮件一起删
    pub fn set_folders(&self, account: i64, folders: &[(String, Option<&str>)]) -> Result<()> {
        let tx = self.0.unchecked_transaction()?;
        for f in self.folders(account)? {
            if !folders.iter().any(|(n, _)| *n == f.name) {
                tx.execute("DELETE FROM folder WHERE id = ?1", [f.id])?;
            }
        }
        for (name, role) in folders {
            tx.execute(
                "INSERT INTO folder (account, name, role) VALUES (?1, ?2, ?3)
                 ON CONFLICT (account, name) DO UPDATE SET role = ?3",
                params![account, name, role],
            )?;
        }
        tx.commit()
    }

    /// 系统文件夹按固定顺序在前，自建文件夹按名字接在后面
    pub fn folders(&self, account: i64) -> Result<Vec<Folder>> {
        let mut st = self.0.prepare(
            "SELECT id, name, uidvalidity, modseq, complete, role FROM folder WHERE account = ?1
             ORDER BY CASE role WHEN 'inbox' THEN 0 WHEN 'flagged' THEN 1 WHEN 'important' THEN 2 WHEN 'sent' THEN 3
             WHEN 'drafts' THEN 4 WHEN 'all' THEN 5 WHEN 'archive' THEN 5 WHEN 'junk' THEN 6 WHEN 'trash' THEN 7
             ELSE 8 END, name",
        )?;
        st.query_map([account], |r| {
            Ok(Folder {
                id: r.get(0)?,
                name: r.get(1)?,
                uidvalidity: r.get(2)?,
                modseq: r.get(3)?,
                complete: r.get(4)?,
                role: r.get(5)?,
            })
        })?
        .collect()
    }

    /// UIDVALIDITY 变了说明服务器重新编了号，这个文件夹的缓存全部作废
    pub fn reset_folder(&self, folder: i64, uidvalidity: u32) -> Result<()> {
        let tx = self.0.unchecked_transaction()?;
        tx.execute("DELETE FROM message WHERE folder = ?1", [folder])?;
        tx.execute(
            "UPDATE folder SET uidvalidity = ?2, modseq = NULL, complete = 0 WHERE id = ?1",
            params![folder, uidvalidity],
        )?;
        tx.commit()
    }

    pub fn finish_folder(&self, folder: i64, modseq: Option<u64>) -> Result<()> {
        self.0.execute("UPDATE folder SET modseq = ?2, complete = 1 WHERE id = ?1", params![folder, modseq])?;
        Ok(())
    }

    pub fn uids(&self, folder: i64) -> Result<HashSet<u32>> {
        let mut st = self.0.prepare("SELECT uid FROM message WHERE folder = ?1")?;
        st.query_map([folder], |r| r.get(0))?.collect()
    }

    pub fn max_uid(&self, folder: i64) -> Result<u32> {
        self.0.query_row("SELECT COALESCE(MAX(uid), 0) FROM message WHERE folder = ?1", [folder], |r| r.get(0))
    }

    /// (总数, 未读数)
    pub fn stats(&self, folder: i64) -> Result<(u32, u32)> {
        self.0.query_row(
            "SELECT COUNT(*), COALESCE(SUM(flags & 1 = 0), 0) FROM message WHERE folder = ?1",
            [folder],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    }

    /// 最新一封的收到时间（INTERNALDATE），空文件夹为 0
    pub fn latest(&self, folder: i64) -> Result<i64> {
        self.0.query_row("SELECT COALESCE(MAX(date), 0) FROM message WHERE folder = ?1", [folder], |r| r.get(0))
    }

    /// 返回新增封数；已有的不覆盖，标记由 [`Store::set_flags`] 更新
    pub fn put_headers(&self, folder: i64, headers: &[Header]) -> Result<usize> {
        let tx = self.0.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut st = tx.prepare(
                "INSERT OR IGNORE INTO message VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for h in headers {
                n += st.execute(params![
                    folder, h.uid, h.flags, h.date, h.size, h.attach, h.from_name, h.from_addr, h.subject
                ])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// 用服务器的标记更新本地，有待写回改动的邮件以本地为准；返回实际变了的封数
    pub fn set_flags(&self, folder: i64, flags: &[(u32, u8)]) -> Result<usize> {
        let tx = self.0.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut st = tx.prepare(
                "UPDATE message SET flags = ?3 WHERE folder = ?1 AND uid = ?2 AND flags <> ?3
                 AND NOT EXISTS (SELECT 1 FROM pending WHERE folder = ?1 AND uid = ?2)",
            )?;
            for (uid, f) in flags {
                n += st.execute(params![folder, uid, f])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// 删掉本地的这些邮件（服务器上已经没有，或界面上刚移走），返回删掉的封数
    pub fn remove(&self, folder: i64, uids: &[u32]) -> Result<usize> {
        let tx = self.0.unchecked_transaction()?;
        let mut n = 0;
        for uid in uids {
            n += tx.execute("DELETE FROM message WHERE folder = ?1 AND uid = ?2", params![folder, uid])?;
        }
        tx.commit()?;
        Ok(n)
    }

    /// 最新的 `limit` 封
    pub fn list(&self, folder: i64, limit: u32) -> Result<Vec<Header>> {
        let mut st = self.0.prepare(&format!("SELECT {HEADER} FROM message WHERE folder = ?1 ORDER BY date DESC LIMIT ?2"))?;
        st.query_map(params![folder, limit], header)?.collect()
    }

    /// 这些文件夹里主题、发件人名字或地址含有每一个词的邮件，新的在前；不分大小写只限英文字母
    pub fn search(&self, folders: &[i64], words: &[&str]) -> Result<Vec<Header>> {
        let ids: Vec<String> = folders.iter().map(i64::to_string).collect();
        let mut sql = format!("SELECT {HEADER} FROM message WHERE folder IN ({})", ids.join(","));
        for i in 1..=words.len() {
            sql += &format!(" AND (subject LIKE ?{i} ESCAPE '\\' OR from_name LIKE ?{i} ESCAPE '\\' OR from_addr LIKE ?{i} ESCAPE '\\')");
        }
        let like = words.iter().map(|w| format!("%{}%", w.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")));
        self.0.prepare(&(sql + " ORDER BY date DESC"))?.query_map(rusqlite::params_from_iter(like), header)?.collect()
    }

    /// 本地设上 / 去掉一个标记（[`SEEN`]、[`FLAGGED`]），同时记待写回，下次同步这个文件夹时写回服务器
    pub fn mark(&self, folder: i64, uids: &[u32], flag: u8, on: bool) -> Result<()> {
        let tx = self.0.unchecked_transaction()?;
        let bits = if on { flag } else { 0 };
        {
            let mut set = tx.prepare("UPDATE message SET flags = flags & ~?3 | ?4 WHERE folder = ?1 AND uid = ?2")?;
            let mut add = tx.prepare(
                "INSERT INTO pending (folder, uid, bits, mask) VALUES (?1, ?2, ?4, ?3)
                 ON CONFLICT DO UPDATE SET bits = bits & ~?3 | ?4, mask = mask | ?3",
            )?;
            for uid in uids {
                set.execute(params![folder, uid, flag, bits])?;
                add.execute(params![folder, uid, flag, bits])?;
            }
        }
        tx.commit()
    }

    /// (uid, 改过的标记位, 它们的新值)
    pub fn pending(&self, folder: i64) -> Result<Vec<(u32, u8, u8)>> {
        let mut st = self.0.prepare("SELECT uid, mask, bits FROM pending WHERE folder = ?1")?;
        st.query_map([folder], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect()
    }

    /// 标记 `flag` 已写回成 `on`，从待写回里去掉；写回期间又被改过的留到下次
    pub fn clear_pending(&self, folder: i64, uids: &[u32], flag: u8, on: bool) -> Result<()> {
        let tx = self.0.unchecked_transaction()?;
        for uid in uids {
            tx.execute(
                "UPDATE pending SET mask = mask & ~?3, bits = bits & ~?3
                 WHERE folder = ?1 AND uid = ?2 AND (bits & ?3 <> 0) = ?4",
                params![folder, uid, flag, on],
            )?;
            tx.execute("DELETE FROM pending WHERE folder = ?1 AND uid = ?2 AND mask = 0", params![folder, uid])?;
        }
        tx.commit()
    }

    pub fn body(&self, folder: i64, uid: u32) -> Result<Option<Vec<u8>>> {
        self.0
            .query_row("SELECT raw FROM body WHERE folder = ?1 AND uid = ?2", params![folder, uid], |r| r.get(0))
            .optional()
    }

    pub fn put_body(&self, folder: i64, uid: u32, raw: &[u8]) -> Result<()> {
        self.0.execute("INSERT OR REPLACE INTO body VALUES (?1, ?2, ?3)", params![folder, uid, raw])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(uid: u32) -> Header {
        Header {
            folder: 0,
            uid,
            flags: 0,
            date: uid as i64,
            size: 1,
            attach: false,
            from_name: String::new(),
            from_addr: String::new(),
            subject: format!("m{uid}"),
        }
    }

    #[test]
    fn pending_wins_over_server_flags() {
        let db = Store::open(":memory:").unwrap();
        let proxy = Some("127.0.0.1:7898".to_string());
        let smtp_host = "smtp.x".to_string();
        db.add_account(&Account { email: "a@x".into(), host: "imap.x".into(), port: 993, proxy, name: "A".into(), smtp_host, smtp_port: 465, id: 0 }).unwrap();
        let acc = db.accounts().unwrap()[0].id;
        db.set_proxy(acc, None).unwrap();
        let a = &db.accounts().unwrap()[0];
        assert_eq!((a.proxy.as_deref(), a.name.as_str(), a.smtp_host.as_str(), a.smtp_port), (None, "A", "smtp.x", 465));
        db.set_folders(acc, &[("INBOX".into(), Some("inbox"))]).unwrap();
        let f = db.folders(acc).unwrap()[0].id;
        assert_eq!(db.put_headers(f, &[header(1), header(2)]).unwrap(), 2);
        assert_eq!(db.put_headers(f, &[header(2), header(3)]).unwrap(), 1);

        db.mark(f, &[1], SEEN, true).unwrap();
        db.mark(f, &[1], FLAGGED, true).unwrap();
        // 服务器还是未读：待写回的 1 不被覆盖，2 照常更新
        assert_eq!(db.set_flags(f, &[(1, 0), (2, SEEN | FLAGGED)]).unwrap(), 1);
        assert_eq!(db.stats(f).unwrap(), (3, 1));
        // 写回期间又取消了星标：已读清掉，星标留到下次
        db.mark(f, &[1], FLAGGED, false).unwrap();
        db.clear_pending(f, &[1], SEEN, false).unwrap();
        db.clear_pending(f, &[1], SEEN, true).unwrap();
        db.clear_pending(f, &[1], FLAGGED, true).unwrap();
        assert_eq!(db.pending(f).unwrap(), [(1, FLAGGED, 0)]);
        assert_eq!(db.list(f, 9).unwrap().iter().find(|h| h.uid == 1).unwrap().flags, SEEN);
        db.clear_pending(f, &[1], FLAGGED, false).unwrap();
        assert!(db.pending(f).unwrap().is_empty());
        db.mark(f, &[1], SEEN, false).unwrap();
        assert_eq!(db.pending(f).unwrap(), [(1, SEEN, 0)]);

        // 删邮件连带待写回和正文
        db.put_body(f, 1, b"raw").unwrap();
        assert_eq!(db.remove(f, &[1, 9]).unwrap(), 1);
        assert!(db.pending(f).unwrap().is_empty() && db.body(f, 1).unwrap().is_none());

        // 测试邮件的收到时间等于 uid
        assert_eq!(db.latest(f).unwrap(), 3);
        db.reset_folder(f, 7).unwrap();
        assert_eq!((db.max_uid(f).unwrap(), db.latest(f).unwrap()), (0, 0));
        db.set_folders(acc, &[]).unwrap();
        assert!(db.folders(acc).unwrap().is_empty());
    }

    #[test]
    fn search_matches_every_word() {
        let db = Store::open(":memory:").unwrap();
        db.add_account(&Account { email: "a@x".into(), ..Default::default() }).unwrap();
        let acc = db.accounts().unwrap()[0].id;
        db.set_folders(acc, &[("INBOX".into(), Some("inbox")), ("Work".into(), None)]).unwrap();
        let id = |name: &str| db.folders(acc).unwrap().into_iter().find(|f| f.name == name).unwrap().id;
        let (inbox, work) = (id("INBOX"), id("Work"));
        let mail = |uid, from_name: &str, from_addr: &str, subject: &str| Header {
            from_name: from_name.into(),
            from_addr: from_addr.into(),
            subject: subject.into(),
            ..header(uid)
        };
        db.put_headers(inbox, &[mail(1, "Li Si", "li@x", "周报 第三周"), mail(2, "", "shop@x", "100% off_sale"), mail(3, "张三", "z@x", "周报")]).unwrap();
        db.put_headers(work, &[mail(4, "Wang", "w@x", "项目周报")]).unwrap();
        let uids = |folders: &[i64], words: &[&str]| db.search(folders, words).unwrap().iter().map(|h| (h.folder, h.uid)).collect::<Vec<_>>();

        // 新的在前；每个词都要匹配上，可以分别在主题和发件人里
        assert_eq!(uids(&[inbox], &["周报"]), [(inbox, 3), (inbox, 1)]);
        assert_eq!(uids(&[inbox], &["周报", "li"]), [(inbox, 1)]);
        assert_eq!(uids(&[inbox], &["LI@X"]), [(inbox, 1)]);
        // % 和 _ 按字面匹配
        assert_eq!(uids(&[inbox], &["%"]), [(inbox, 2)]);
        assert!(uids(&[inbox], &["o_f"]).is_empty());
        // 只搜给定的文件夹
        assert_eq!(uids(&[inbox, work], &["周报"]), [(work, 4), (inbox, 3), (inbox, 1)]);
        assert_eq!(uids(&[work], &["周报"]), [(work, 4)]);
    }
}
