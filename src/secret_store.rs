use crate::native_interop;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Security::Credentials::{
    CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE,
    CRED_TYPE_GENERIC,
};

const GITHUB_PAT_TARGET: &str = "OpencodeUsageMonitor/GitHubPAT";
const GITHUB_PAT_USERNAME: &str = "github-pat";

pub fn has_github_pat() -> bool {
    load_github_pat().is_some()
}

pub fn load_github_pat() -> Option<String> {
    let target = native_interop::wide_str(GITHUB_PAT_TARGET);
    let mut credential = std::ptr::null_mut();

    unsafe {
        if CredReadW(
            PCWSTR::from_raw(target.as_ptr()),
            CRED_TYPE_GENERIC,
            0,
            &mut credential,
        )
        .is_err()
        {
            return None;
        }

        let token = credential
            .as_ref()
            .and_then(|cred| {
                if cred.CredentialBlob.is_null() || cred.CredentialBlobSize == 0 {
                    return None;
                }

                let bytes = std::slice::from_raw_parts(
                    cred.CredentialBlob,
                    cred.CredentialBlobSize as usize,
                );
                String::from_utf8(bytes.to_vec()).ok()
            })
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

        if !credential.is_null() {
            CredFree(credential.cast());
        }

        token
    }
}

pub fn save_github_pat(token: &str) -> Result<(), String> {
    let token = token.trim();
    if token.is_empty() {
        return Err("GitHub token cannot be empty.".to_string());
    }

    let target = native_interop::wide_str(GITHUB_PAT_TARGET);
    let username = native_interop::wide_str(GITHUB_PAT_USERNAME);
    let mut blob = token.as_bytes().to_vec();

    let credential = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target.as_ptr() as *mut _),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        UserName: PWSTR(username.as_ptr() as *mut _),
        ..Default::default()
    };

    unsafe {
        CredWriteW(&credential, 0).map_err(|error| {
            format!("Unable to save GitHub token to Windows Credential Manager: {error}")
        })
    }
}

pub fn clear_github_pat() -> Result<(), String> {
    if !has_github_pat() {
        return Ok(());
    }

    let target = native_interop::wide_str(GITHUB_PAT_TARGET);
    unsafe {
        CredDeleteW(PCWSTR::from_raw(target.as_ptr()), CRED_TYPE_GENERIC, 0).map_err(|error| {
            format!("Unable to remove GitHub token from Windows Credential Manager: {error}")
        })
    }
}
