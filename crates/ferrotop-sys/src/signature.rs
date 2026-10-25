//! Authenticode verification: whether a file is signed by a trusted publisher, and who.
//!
//! Like System Informer's `PhVerifyFile`, this checks an embedded signature first and falls back
//! to the system catalogs, which is how most Windows files are signed. Revocation checks are off
//! and only cached URLs are used, so verification never waits on the network.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::{GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Cryptography::Catalog::{
    CATALOG_INFO, CryptCATAdminAcquireContext2, CryptCATAdminCalcHashFromFileHandle2, CryptCATAdminEnumCatalogFromHash,
    CryptCATAdminReleaseCatalogContext, CryptCATAdminReleaseContext, CryptCATCatalogInfoFromContext,
};
use windows_sys::Win32::Security::Cryptography::{CERT_NAME_SIMPLE_DISPLAY_TYPE, CertGetNameStringW};
use windows_sys::Win32::Security::WinTrust::{
    WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_CATALOG_INFO, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
    WTD_CACHE_ONLY_URL_RETRIEVAL, WTD_CHOICE_CATALOG, WTD_CHOICE_FILE, WTD_REVOCATION_CHECK_NONE, WTD_REVOKE_NONE,
    WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE, WTHelperGetProvSignerFromChain,
    WTHelperProvDataFromStateData, WinVerifyTrust,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

use crate::buffer::wide;

const TRUST_E_NOSIGNATURE: i32 = 0x800B_0100_u32 as i32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signature {
    /// Signed by a trusted publisher; the signer's display name.
    Verified(String),
    /// No embedded signature and not in any system catalog.
    Unsigned,
    /// Signed, but the signature didn't verify (expired, untrusted root, tampered...).
    Untrusted(i32),
}

/// Verifies `path`'s Authenticode signature (embedded, then catalog).
pub fn verify(path: &str) -> Signature {
    match verify_embedded(path) {
        (0, signer) => Signature::Verified(signer.unwrap_or_default()),
        (TRUST_E_NOSIGNATURE, _) => match verify_catalog(path) {
            Some((0, signer)) => Signature::Verified(signer.unwrap_or_default()),
            Some((status, _)) if status != TRUST_E_NOSIGNATURE => Signature::Untrusted(status),
            _ => Signature::Unsigned,
        },
        (status, _) => Signature::Untrusted(status),
    }
}

fn trust_data(choice: u32, target: WINTRUST_DATA_0) -> WINTRUST_DATA {
    WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        pPolicyCallbackData: std::ptr::null_mut(),
        pSIPClientData: std::ptr::null_mut(),
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: choice,
        Anonymous: target,
        dwStateAction: WTD_STATEACTION_VERIFY,
        hWVTStateData: std::ptr::null_mut(),
        pwszURLReference: std::ptr::null_mut(),
        dwProvFlags: WTD_REVOCATION_CHECK_NONE | WTD_CACHE_ONLY_URL_RETRIEVAL,
        dwUIContext: 0,
        pSignatureSettings: std::ptr::null_mut(),
    }
}

/// Runs a verification and reads the signer, always closing the verification state.
fn run(data: &mut WINTRUST_DATA) -> (i32, Option<String>) {
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // SAFETY: INVALID_HANDLE_VALUE as the window means "no UI"; `action` and `data` are valid,
    // and `data`'s union points at a structure that outlives this call.
    let status = unsafe { WinVerifyTrust(INVALID_HANDLE_VALUE, &mut action, (data as *mut WINTRUST_DATA).cast()) };
    let signer = if status == 0 { signer_name(data.hWVTStateData) } else { None };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    // SAFETY: as above; this releases the state the verify call opened.
    unsafe { WinVerifyTrust(INVALID_HANDLE_VALUE, &mut action, (data as *mut WINTRUST_DATA).cast()) };
    (status, signer)
}

/// The signing certificate's display name, read from an open verification state.
fn signer_name(state: HANDLE) -> Option<String> {
    // SAFETY: `state` comes from a successful verify and is still open.
    let provider = unsafe { WTHelperProvDataFromStateData(state) };
    if provider.is_null() {
        return None;
    }
    // SAFETY: `provider` is valid while the state is open.
    let signer = unsafe { WTHelperGetProvSignerFromChain(provider, 0, 0, 0) };
    if signer.is_null() {
        return None;
    }
    // SAFETY: `signer` points at a CRYPT_PROVIDER_SGNR owned by the open state.
    let signer = unsafe { &*signer };
    if signer.csCertChain == 0 || signer.pasCertChain.is_null() {
        return None;
    }
    // SAFETY: the chain has `csCertChain` (≥ 1) entries; the first is the signing certificate.
    let cert = unsafe { (*signer.pasCertChain).pCert };
    if cert.is_null() {
        return None;
    }
    let mut name = [0u16; 256];
    // SAFETY: `cert` is a valid CERT_CONTEXT and `name` is writable for 256 characters.
    let len = unsafe {
        CertGetNameStringW(
            cert,
            CERT_NAME_SIMPLE_DISPLAY_TYPE,
            0,
            std::ptr::null(),
            name.as_mut_ptr(),
            name.len() as u32,
        )
    };
    // The length includes the terminating NUL.
    (len > 1).then(|| String::from_utf16_lossy(&name[..(len as usize - 1).min(name.len())]))
}

fn verify_embedded(path: &str) -> (i32, Option<String>) {
    let path = wide(path);
    let mut file = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: path.as_ptr(),
        hFile: std::ptr::null_mut(),
        pgKnownSubject: std::ptr::null_mut(),
    };
    run(&mut trust_data(WTD_CHOICE_FILE, WINTRUST_DATA_0 { pFile: &mut file }))
}

struct CatalogAdmin(isize);
impl Drop for CatalogAdmin {
    fn drop(&mut self) {
        // SAFETY: the context came from CryptCATAdminAcquireContext2 and is released exactly once.
        unsafe { CryptCATAdminReleaseContext(self.0, 0) };
    }
}

struct CatalogInfo<'a>(&'a CatalogAdmin, isize);
impl Drop for CatalogInfo<'_> {
    fn drop(&mut self) {
        // SAFETY: the catalog context came from CryptCATAdminEnumCatalogFromHash on this admin
        // context and is released exactly once.
        unsafe { CryptCATAdminReleaseCatalogContext(self.0.0, self.1, 0) };
    }
}

/// `None` when the file can't be opened or hashed; otherwise the catalog verification result
/// (`TRUST_E_NOSIGNATURE` when no catalog lists the file).
fn verify_catalog(path: &str) -> Option<(i32, Option<String>)> {
    let path = wide(path);
    // SAFETY: `path` is NUL-terminated; the other arguments are plain flags or null.
    let raw = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return None;
    }
    // SAFETY: `raw` is a handle we just opened, and nothing else owns it.
    let file = unsafe { OwnedHandle::from_raw_handle(raw) };

    let algorithm = wide("SHA256");
    let mut admin = 0isize;
    // SAFETY: `admin` is a valid out-pointer and `algorithm` is NUL-terminated.
    if unsafe { CryptCATAdminAcquireContext2(&mut admin, std::ptr::null(), algorithm.as_ptr(), std::ptr::null(), 0) }
        == 0
    {
        return None;
    }
    let admin = CatalogAdmin(admin);

    let mut hash_len = 0u32;
    // SAFETY: a null buffer asks for the hash size; `file` is a live handle.
    unsafe {
        CryptCATAdminCalcHashFromFileHandle2(admin.0, file.as_raw_handle(), &mut hash_len, std::ptr::null_mut(), 0)
    };
    if hash_len == 0 || hash_len > 64 {
        return None;
    }
    let mut hash = vec![0u8; hash_len as usize];
    // SAFETY: `hash` is writable for `hash_len` bytes and `file` is a live handle.
    if unsafe {
        CryptCATAdminCalcHashFromFileHandle2(admin.0, file.as_raw_handle(), &mut hash_len, hash.as_mut_ptr(), 0)
    } == 0
    {
        return None;
    }

    // SAFETY: `hash` holds `hash_len` bytes; a null previous context starts the search.
    let catalog =
        unsafe { CryptCATAdminEnumCatalogFromHash(admin.0, hash.as_ptr(), hash_len, 0, std::ptr::null_mut()) };
    if catalog == 0 {
        return Some((TRUST_E_NOSIGNATURE, None));
    }
    let catalog = CatalogInfo(&admin, catalog);
    let mut info = CATALOG_INFO { cbStruct: size_of::<CATALOG_INFO>() as u32, wszCatalogFile: [0; 260] };
    // SAFETY: `catalog` is a live catalog context and `info` is a writable CATALOG_INFO.
    if unsafe { CryptCATCatalogInfoFromContext(catalog.1, &mut info, 0) } == 0 {
        return None;
    }

    // Catalog members are tagged with the file hash in upper-case hex.
    let tag = wide(&hash.iter().map(|b| format!("{b:02X}")).collect::<String>());
    let mut member = WINTRUST_CATALOG_INFO {
        cbStruct: size_of::<WINTRUST_CATALOG_INFO>() as u32,
        dwCatalogVersion: 0,
        pcwszCatalogFilePath: info.wszCatalogFile.as_ptr(),
        pcwszMemberTag: tag.as_ptr(),
        pcwszMemberFilePath: path.as_ptr(),
        hMemberFile: file.as_raw_handle(),
        pbCalculatedFileHash: hash.as_mut_ptr(),
        cbCalculatedFileHash: hash_len,
        pcCatalogContext: std::ptr::null_mut(),
        hCatAdmin: admin.0,
    };
    Some(run(&mut trust_data(WTD_CHOICE_CATALOG, WINTRUST_DATA_0 { pCatalog: &mut member })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_binaries_verify_through_the_catalog() {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        match verify(&format!(r"{root}\System32\notepad.exe")) {
            Signature::Verified(signer) => assert!(signer.contains("Microsoft"), "signer {signer}"),
            other => panic!("notepad.exe should verify, got {other:?}"),
        }
    }

    #[test]
    fn unsigned_files_report_unsigned() {
        let path = std::env::temp_dir().join("ferrotop-unsigned-test.exe");
        std::fs::write(&path, b"MZ not really a program").unwrap();
        let result = verify(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        assert!(matches!(result, Signature::Unsigned | Signature::Untrusted(_)), "{result:?}");
    }
}
