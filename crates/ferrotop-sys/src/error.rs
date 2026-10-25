use std::fmt;

/// A failed OS call: which API failed and the status or error code it returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// An `Nt*` call returned a failing NTSTATUS.
    Nt { call: &'static str, status: i32 },
    /// A Win32 call failed; `code` is the `GetLastError` value.
    Win32 { call: &'static str, code: u32 },
    /// A COM-style call returned a failing HRESULT.
    HResult { call: &'static str, hresult: i32 },
    /// The kernel returned data that failed validation.
    Malformed { call: &'static str },
    /// The process exited and its PID now belongs to a different process.
    ProcessGone,
    /// Something else went wrong; the message says what, e.g. "couldn't copy Ferrotop to
    /// Program Files".
    Other { message: &'static str },
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) fn last_win32(call: &'static str) -> Self {
        let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        Error::Win32 { call, code: code as u32 }
    }

    /// True when the failure was an access check, the usual result for protected processes.
    pub fn is_access_denied(&self) -> bool {
        const ERROR_ACCESS_DENIED: u32 = 5;
        const STATUS_ACCESS_DENIED: i32 = 0xC000_0022_u32 as i32;
        matches!(self, Error::Win32 { code: ERROR_ACCESS_DENIED, .. } | Error::Nt { status: STATUS_ACCESS_DENIED, .. })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Error::Nt { call, status } => write!(f, "{call} failed (NTSTATUS 0x{:08X})", status as u32),
            Error::Win32 { call, code } => {
                write!(f, "{call} failed: {}", std::io::Error::from_raw_os_error(code as i32))
            }
            Error::HResult { call, hresult } => write!(f, "{call} failed (HRESULT 0x{:08X})", hresult as u32),
            Error::Malformed { call } => write!(f, "{call} returned malformed data"),
            Error::ProcessGone => f.write_str("the process has already exited"),
            Error::Other { message } => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}
