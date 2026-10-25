//! 账号密码 / 令牌存在 Windows 凭据管理器（只存本机，不漫游），不进数据库。

use windows::Win32::Security::Credentials::{
    CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredFree, CredReadW, CredWriteW,
};
use windows::core::{HSTRING, PWSTR, Result};

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

#[cfg(test)]
mod tests {
    use windows::Win32::Security::Credentials::CredDeleteW;

    #[test]
    fn round_trip() {
        let email = "cred-test@mailpro.invalid";
        super::save(email, "密码 pw").unwrap();
        assert_eq!(super::load(email).unwrap(), "密码 pw");
        unsafe { CredDeleteW(&super::target(email), super::CRED_TYPE_GENERIC, None) }.unwrap();
        assert!(super::load(email).is_err());
    }
}
