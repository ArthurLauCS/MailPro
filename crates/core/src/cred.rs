//! 账号密码 / 令牌存在 Windows 凭据管理器（只存本机，不漫游），不进数据库。

use windows::Win32::Foundation::ERROR_NOT_FOUND;
use windows::Win32::Security::Credentials::{
    CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredEnumerateW, CredFree, CredReadW, CredWriteW,
};
use windows::core::{HSTRING, PCWSTR, PWSTR, Result};

fn target(email: &str) -> HSTRING {
    format!("MailPro/{email}").into()
}

pub fn save(email: &str, secret: &str) -> Result<()> {
    let target = target(email);
    let cred = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target.as_ptr() as *mut _),
        CredentialBlobSize: secret.len() as u32,
        CredentialBlob: secret.as_ptr() as *mut _,
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        ..Default::default()
    };
    unsafe { CredWriteW(&cred, 0) }
}

pub fn load(email: &str) -> Result<String> {
    let mut p = std::ptr::null_mut();
    unsafe {
        CredReadW(&target(email), CRED_TYPE_GENERIC, None, &mut p)?;
        let blob = std::slice::from_raw_parts((*p).CredentialBlob, (*p).CredentialBlobSize as usize);
        let secret = String::from_utf8_lossy(blob).into_owned();
        CredFree(p as _);
        Ok(secret)
    }
}

/// 删掉本程序存的全部凭据（邮箱密码、AdsPower key），卸载程序调用
pub fn purge() -> Result<()> {
    delete_matching(&target("*"))
}

fn delete_matching(filter: &HSTRING) -> Result<()> {
    let (mut n, mut list) = (0, std::ptr::null_mut());
    unsafe {
        match CredEnumerateW(filter, None, &mut n, &mut list) {
            Err(e) if e.code() == ERROR_NOT_FOUND.to_hresult() => return Ok(()),
            r => r?,
        }
        let r = std::slice::from_raw_parts(list, n as usize)
            .iter()
            .try_for_each(|c| CredDeleteW(PCWSTR((**c).TargetName.0), CRED_TYPE_GENERIC, None));
        CredFree(list as _);
        r
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::Security::Credentials::CredDeleteW;

    #[test]
    fn delete_matching() {
        let emails = ["purge-test-1@mailpro.invalid", "purge-test-2@mailpro.invalid"];
        for e in emails {
            super::save(e, "pw").unwrap();
        }
        super::delete_matching(&super::target("purge-test-*")).unwrap();
        assert!(emails.iter().all(|e| super::load(e).is_err()));
        // 没有匹配的也算成功
        super::delete_matching(&super::target("purge-test-*")).unwrap();
    }

    #[test]
    fn round_trip() {
        let email = "cred-test@mailpro.invalid";
        super::save(email, "密码 pw").unwrap();
        assert_eq!(super::load(email).unwrap(), "密码 pw");
        unsafe { CredDeleteW(&super::target(email), super::CRED_TYPE_GENERIC, None) }.unwrap();
        assert!(super::load(email).is_err());
    }
}
