//! Process tokens: who a process runs as, and whether it's elevated.

use std::fmt;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
use windows_sys::Win32::Foundation::LUID;
use windows_sys::Win32::Security::{
    GetTokenInformation, LookupAccountSidW, LookupPrivilegeNameW, SID_NAME_USE, TOKEN_ELEVATION,
    TOKEN_INFORMATION_CLASS, TOKEN_QUERY, TokenElevation, TokenElevationType, TokenGroups, TokenIntegrityLevel,
    TokenPrivileges, TokenSessionId, TokenUser, TokenVirtualizationAllowed, TokenVirtualizationEnabled,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION};

use crate::buffer::{AlignedBuffer, offset_in, read_pod};
use crate::ntapi::SidAndAttributes;
use crate::process::open_process;
use crate::{Error, Result};

/// A security identifier, stored as its validated binary form.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Sid(Vec<u8>);

impl Sid {
    /// `S-1-5-18`, the LocalSystem account.
    pub fn local_system() -> Self {
        Sid(vec![1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0])
    }

    /// Parses a binary SID at the start of `bytes`, checking it fits.
    pub(crate) fn parse(bytes: &[u8]) -> Option<Self> {
        let &[revision, count, ..] = bytes else { return None };
        if revision != 1 || count > 15 {
            return None;
        }
        Some(Sid(bytes.get(..8 + 4 * usize::from(count))?.to_vec()))
    }

    fn sub_authorities(&self) -> impl Iterator<Item = u32> + '_ {
        self.0[8..].as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
    }

    fn authority(&self) -> u64 {
        self.0[2..8].iter().fold(0, |acc, &b| (acc << 8) | u64::from(b))
    }

    /// LocalSystem, LocalService or NetworkService: the accounts Windows' own services run as.
    pub fn is_service_account(&self) -> bool {
        self.authority() == 5 && matches!(self.sub_authorities().collect::<Vec<_>>()[..], [18] | [19] | [20])
    }
}

impl fmt::Display for Sid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "S-{}-{}", self.0[0], self.authority())?;
        self.sub_authorities().try_for_each(|sub| write!(f, "-{sub}"))
    }
}

impl fmt::Debug for Sid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sid({self})")
    }
}

/// The parts of a process token Ferrotop shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenInfo {
    pub user: Sid,
    pub elevated: bool,
}

/// Token details for another process.
pub fn process_token(pid: u32) -> Result<TokenInfo> {
    let process = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    token_info(&open_token(&process)?)
}

/// Token details for Ferrotop itself.
pub fn current_token() -> Result<TokenInfo> {
    // SAFETY: returns a pseudo-handle that needs no closing; no preconditions.
    let process = unsafe { GetCurrentProcess() };
    let mut token = std::ptr::null_mut();
    // SAFETY: `process` is the current-process pseudo-handle and `token` is a valid out-pointer.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(Error::last_win32("OpenProcessToken"));
    }
    // SAFETY: `token` is a handle we just opened, and nothing else owns it.
    token_info(&unsafe { OwnedHandle::from_raw_handle(token) })
}

fn open_token(process: &OwnedHandle) -> Result<OwnedHandle> {
    let mut token = std::ptr::null_mut();
    // SAFETY: `process` is a live process handle and `token` is a valid out-pointer.
    if unsafe { OpenProcessToken(process.as_raw_handle(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(Error::last_win32("OpenProcessToken"));
    }
    // SAFETY: `token` is a handle we just opened, and nothing else owns it.
    Ok(unsafe { OwnedHandle::from_raw_handle(token) })
}

fn token_info(token: &OwnedHandle) -> Result<TokenInfo> {
    let (buffer, len) = query_token(token, TokenUser)?;
    let bytes = buffer.bytes(len);
    let malformed = || Error::Malformed { call: "GetTokenInformation(TokenUser)" };
    let user: SidAndAttributes = read_pod(bytes, 0).ok_or_else(malformed)?;
    let sid = Sid::parse(&bytes[offset_in(bytes, user.sid).ok_or_else(malformed)?..]).ok_or_else(malformed)?;

    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut written = 0u32;
    // SAFETY: `token` is live and `elevation` is a writable TOKEN_ELEVATION of the size passed.
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenElevation,
            (&raw mut elevation).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut written,
        )
    };
    Ok(TokenInfo { user: sid, elevated: ok != 0 && elevation.TokenIsElevated != 0 })
}

/// A group in a token, with its `SE_GROUP_*` attribute flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenGroup {
    pub sid: Sid,
    pub attributes: u32,
}

/// A privilege in a token, with its `SE_PRIVILEGE_*` attribute flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenPrivilege {
    pub name: String,
    pub attributes: u32,
}

/// Everything the token page shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenDetails {
    pub user: Sid,
    pub session_id: u32,
    pub elevated: bool,
    /// 1 default (UAC off or not an admin), 2 full (elevated), 3 limited (filtered admin).
    pub elevation_type: u32,
    /// The mandatory label, e.g. `S-1-16-12288` (High).
    pub integrity: Option<Sid>,
    pub virtualization_allowed: bool,
    pub virtualization_enabled: bool,
    pub groups: Vec<TokenGroup>,
    pub privileges: Vec<TokenPrivilege>,
}

/// The full token of process `pid`.
pub fn token_details(pid: u32) -> Result<TokenDetails> {
    let process = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let token = open_token(&process)?;
    let basics = token_info(&token)?;
    let dword = |class| query_token(&token, class).ok().and_then(|(b, len)| read_pod::<u32>(b.bytes(len), 0));

    let groups =
        query_token(&token, TokenGroups).ok().and_then(|(b, len)| parse_groups(b.bytes(len))).unwrap_or_default();
    let integrity = query_token(&token, TokenIntegrityLevel).ok().and_then(|(b, len)| {
        let bytes = b.bytes(len);
        let label: SidAndAttributes = read_pod(bytes, 0)?;
        Sid::parse(&bytes[offset_in(bytes, label.sid)?..])
    });
    let privileges = query_token(&token, TokenPrivileges)
        .ok()
        .and_then(|(b, len)| parse_privileges(b.bytes(len)))
        .unwrap_or_default()
        .into_iter()
        .map(|(luid, attributes)| TokenPrivilege {
            name: privilege_name(luid).unwrap_or_else(|| format!("LUID {luid:#x}")),
            attributes,
        })
        .collect();
    Ok(TokenDetails {
        user: basics.user,
        session_id: dword(TokenSessionId).unwrap_or(0),
        elevated: basics.elevated,
        elevation_type: dword(TokenElevationType).unwrap_or(0),
        integrity,
        virtualization_allowed: dword(TokenVirtualizationAllowed).unwrap_or(0) != 0,
        virtualization_enabled: dword(TokenVirtualizationEnabled).unwrap_or(0) != 0,
        groups,
        privileges,
    })
}

/// `TOKEN_GROUPS`: a count, then (pointer-aligned) `SID_AND_ATTRIBUTES` entries whose SIDs live
/// later in the same buffer.
pub(crate) fn parse_groups(bytes: &[u8]) -> Option<Vec<TokenGroup>> {
    let count: u32 = read_pod(bytes, 0)?;
    let first = size_of::<usize>();
    let fits = bytes.len().saturating_sub(first) / size_of::<SidAndAttributes>();
    (0..(count as usize).min(fits))
        .map(|i| {
            let entry: SidAndAttributes = read_pod(bytes, first + i * size_of::<SidAndAttributes>())?;
            Some(TokenGroup { sid: Sid::parse(&bytes[offset_in(bytes, entry.sid)?..])?, attributes: entry.attributes })
        })
        .collect()
}

/// `TOKEN_PRIVILEGES`: a count, then 12-byte `LUID_AND_ATTRIBUTES` entries.
pub(crate) fn parse_privileges(bytes: &[u8]) -> Option<Vec<(u64, u32)>> {
    let count: u32 = read_pod(bytes, 0)?;
    let entry = |i: usize| {
        let at = 4 + i * 12;
        let field = |offset: usize| {
            bytes.get(at + offset..at + offset + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        Some(((u64::from(field(4)?) << 32) | u64::from(field(0)?), field(8)?))
    };
    (0..(count as usize).min(bytes.len() / 12)).map(entry).collect()
}

/// `SeDebugPrivilege` and friends.
fn privilege_name(luid: u64) -> Option<String> {
    let luid = LUID { LowPart: luid as u32, HighPart: (luid >> 32) as i32 };
    let mut name = [0u16; 128];
    let mut len = name.len() as u32;
    // SAFETY: `luid` is a valid LUID, `name` is writable for `len` characters, and a null system
    // name means the local machine.
    if unsafe { LookupPrivilegeNameW(std::ptr::null(), &luid, name.as_mut_ptr(), &mut len) } == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&name[..(len as usize).min(name.len())]))
}

/// Variable-length `GetTokenInformation`, growing the buffer as needed.
fn query_token(token: &OwnedHandle, class: TOKEN_INFORMATION_CLASS) -> Result<(AlignedBuffer, usize)> {
    let mut buffer = AlignedBuffer::new(256);
    for _ in 0..4 {
        let mut needed = 0u32;
        // SAFETY: `token` is live, the pointer/length pair describes memory owned by `buffer`,
        // and `needed` is a valid out-pointer.
        let ok = unsafe {
            GetTokenInformation(token.as_raw_handle(), class, buffer.as_mut_ptr(), buffer.capacity_u32(), &mut needed)
        };
        if ok != 0 {
            return Ok((buffer, needed as usize));
        }
        let error = Error::last_win32("GetTokenInformation");
        if error != (Error::Win32 { call: "GetTokenInformation", code: ERROR_INSUFFICIENT_BUFFER }) {
            return Err(error);
        }
        buffer = AlignedBuffer::new(needed as usize);
    }
    Err(Error::Malformed { call: "GetTokenInformation" })
}

/// `DOMAIN\name` for a SID. This can be slow (it may ask a domain controller), so cache it.
pub fn account_name(sid: &Sid) -> Result<String> {
    let mut sid_copy = sid.0.clone();
    let mut name = vec![0u16; 256];
    let mut domain = vec![0u16; 256];
    for _ in 0..3 {
        let (mut name_len, mut domain_len) = (name.len() as u32, domain.len() as u32);
        let mut kind: SID_NAME_USE = 0;
        // SAFETY: `sid_copy` holds a validated binary SID, both output buffers are writable for
        // the lengths passed, and every out-pointer is valid. A null system name means local.
        let ok = unsafe {
            LookupAccountSidW(
                std::ptr::null(),
                sid_copy.as_mut_ptr().cast(),
                name.as_mut_ptr(),
                &mut name_len,
                domain.as_mut_ptr(),
                &mut domain_len,
                &mut kind,
            )
        };
        if ok != 0 {
            let name = String::from_utf16_lossy(&name[..(name_len as usize).min(name.len())]);
            let domain = String::from_utf16_lossy(&domain[..(domain_len as usize).min(domain.len())]);
            return Ok(if domain.is_empty() { name } else { format!("{domain}\\{name}") });
        }
        let error = Error::last_win32("LookupAccountSidW");
        if error != (Error::Win32 { call: "LookupAccountSidW", code: ERROR_INSUFFICIENT_BUFFER }) {
            return Err(error);
        }
        name.resize(name_len as usize + 1, 0);
        domain.resize(domain_len as usize + 1, 0);
    }
    Err(Error::Malformed { call: "LookupAccountSidW" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_classifies_sids() {
        assert_eq!(Sid::local_system().to_string(), "S-1-5-18");
        assert!(Sid::local_system().is_service_account());
        let user = Sid::parse(&[1, 2, 0, 0, 0, 0, 0, 5, 21, 0, 0, 0, 0xE9, 3, 0, 0]).unwrap();
        assert_eq!(user.to_string(), "S-1-5-21-1001");
        assert!(!user.is_service_account());
        assert!(Sid::parse(&[1, 2, 0, 0, 0, 0, 0, 5, 21]).is_none(), "truncated SID rejected");
    }

    #[test]
    fn reads_own_token_details() {
        let details = token_details(std::process::id()).expect("token details");
        assert!(!details.groups.is_empty());
        assert!(details.privileges.iter().any(|p| p.name == "SeChangeNotifyPrivilege"));
        let integrity = details.integrity.expect("integrity label").to_string();
        assert!(integrity.starts_with("S-1-16-"), "{integrity}");
    }

    #[test]
    fn token_parsers_never_panic() {
        let mut privileges = vec![0u8; 4 + 24];
        privileges[0] = 2;
        crate::fuzz::mutate(&privileges, 3_000, |bytes| {
            let _ = parse_privileges(bytes);
        });
        let mut groups = vec![0u8; 8 + 32 + 24];
        groups[0] = 2;
        crate::fuzz::mutate(&groups, 3_000, |bytes| {
            let _ = parse_groups(bytes);
        });
    }

    #[test]
    fn reads_own_token_and_names_it() {
        let token = current_token().expect("own token");
        let name = account_name(&token.user).expect("account name");
        assert!(name.contains('\\'), "expected DOMAIN\\user, got {name}");
        let system = account_name(&Sid::local_system()).expect("SYSTEM name");
        assert!(!system.is_empty());
    }
}
