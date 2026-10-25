//! A thread's call stack, walked with the system's dbghelp.dll.
//!
//! dbghelp is loaded from System32 only, is told never to prompt or touch a symbol server
//! (`SYMOPT_SECURE`), and gets System32 as its only search path, so nothing is downloaded and
//! names come from module exports. dbghelp isn't thread-safe, so calls are serialized. The thread
//! is suspended only while its registers and frames are captured.

use std::sync::Mutex;

use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackFrame {
    pub address: u64,
    /// `ntdll.dll!NtWaitForSingleObject+0x14`, or `module.dll+0x1a2b` without a symbol.
    pub symbol: String,
}

static DBGHELP: Mutex<()> = Mutex::new(());

/// The call stack of thread `tid` in process `pid`, innermost frame first.
pub fn thread_stack(pid: u32, tid: u32) -> Result<Vec<StackFrame>> {
    if pid == std::process::id() {
        // Suspending one of our own threads could deadlock us (it may hold a lock we need).
        return Err(Error::Malformed { call: "thread_stack(self)" });
    }
    let _serialized = DBGHELP.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    walk::thread_stack(pid, tid)
}

#[cfg(target_arch = "x86_64")]
mod walk {
    use std::ffi::c_void;
    use std::os::windows::io::{AsRawHandle, OwnedHandle};

    use windows_sys::Win32::Foundation::{FreeLibrary, HANDLE, HMODULE};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        ADDRESS64, AddrModeFlat, CONTEXT, CONTEXT_FULL_AMD64, GetThreadContext, PFUNCTION_TABLE_ACCESS_ROUTINE64,
        PGET_MODULE_BASE_ROUTINE64, STACKFRAME64, SYMBOL_INFOW, SYMOPT_DEFERRED_LOADS, SYMOPT_FAIL_CRITICAL_ERRORS,
        SYMOPT_NO_PROMPTS, SYMOPT_SECURE, SYMOPT_UNDNAME,
    };
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
    use windows_sys::Win32::System::Threading::{
        GetProcessIdOfThread, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ, ResumeThread, SuspendThread,
        THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME,
    };
    use windows_sys::core::BOOL;

    use super::StackFrame;
    use crate::buffer::{AlignedBuffer, wide};
    use crate::modules::{modules, symbolize};
    use crate::process::open_process;
    use crate::threads::open_thread;
    use crate::{Error, Result};

    const MAX_FRAMES: usize = 256;
    const MAX_NAME: u32 = 512;
    const IMAGE_FILE_MACHINE_AMD64: u32 = 0x8664;

    type SymSetOptions = unsafe extern "system" fn(u32) -> u32;
    type SymInitializeW = unsafe extern "system" fn(HANDLE, *const u16, BOOL) -> BOOL;
    type SymCleanup = unsafe extern "system" fn(HANDLE) -> BOOL;
    type SymFromAddrW = unsafe extern "system" fn(HANDLE, u64, *mut u64, *mut SYMBOL_INFOW) -> BOOL;
    type StackWalk64 = unsafe extern "system" fn(
        u32,
        HANDLE,
        HANDLE,
        *mut STACKFRAME64,
        *mut c_void,
        *const c_void,
        PFUNCTION_TABLE_ACCESS_ROUTINE64,
        PGET_MODULE_BASE_ROUTINE64,
        *const c_void,
    ) -> BOOL;

    struct Dbghelp {
        module: HMODULE,
        set_options: SymSetOptions,
        initialize: SymInitializeW,
        cleanup: SymCleanup,
        from_address: SymFromAddrW,
        walk: StackWalk64,
        table_access: PFUNCTION_TABLE_ACCESS_ROUTINE64,
        module_base: PGET_MODULE_BASE_ROUTINE64,
    }

    impl Drop for Dbghelp {
        fn drop(&mut self) {
            // SAFETY: the module came from LoadLibraryExW and is freed once, after its last use.
            unsafe { FreeLibrary(self.module) };
        }
    }

    /// Resolves `name` from `module` as a function pointer of type `F`.
    ///
    /// # Safety
    /// `F` must be the export's true function-pointer type.
    unsafe fn export<F: Copy>(module: HMODULE, name: &std::ffi::CStr) -> Result<F> {
        const { assert!(size_of::<F>() == size_of::<usize>()) };
        // SAFETY: `module` is loaded and `name` is NUL-terminated.
        let address = unsafe { GetProcAddress(module, name.as_ptr().cast()) }
            .ok_or(Error::Malformed { call: "GetProcAddress" })?;
        // SAFETY: the caller guarantees `F` is the export's signature; both are one pointer wide.
        Ok(unsafe { std::mem::transmute_copy::<_, F>(&address) })
    }

    impl Dbghelp {
        fn load() -> Result<Self> {
            let name = wide("dbghelp.dll");
            // SAFETY: `name` is NUL-terminated; the reserved file handle must be null.
            let module = unsafe { LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
            if module.is_null() {
                return Err(Error::last_win32("LoadLibraryExW(dbghelp)"));
            }
            // SAFETY: SymSetOptions(DWORD) -> DWORD.
            let set_options = unsafe { export::<SymSetOptions>(module, c"SymSetOptions") };
            // SAFETY: SymInitializeW(HANDLE, PCWSTR, BOOL) -> BOOL.
            let initialize = unsafe { export::<SymInitializeW>(module, c"SymInitializeW") };
            // SAFETY: SymCleanup(HANDLE) -> BOOL.
            let cleanup = unsafe { export::<SymCleanup>(module, c"SymCleanup") };
            // SAFETY: SymFromAddrW(HANDLE, DWORD64, PDWORD64, PSYMBOL_INFOW) -> BOOL.
            let from_address = unsafe { export::<SymFromAddrW>(module, c"SymFromAddrW") };
            // SAFETY: StackWalk64 with the documented nine parameters.
            let walk = unsafe { export::<StackWalk64>(module, c"StackWalk64") };
            // SAFETY: SymFunctionTableAccess64 matches PFUNCTION_TABLE_ACCESS_ROUTINE64.
            let table_access =
                unsafe { export::<PFUNCTION_TABLE_ACCESS_ROUTINE64>(module, c"SymFunctionTableAccess64") };
            // SAFETY: SymGetModuleBase64 matches PGET_MODULE_BASE_ROUTINE64.
            let module_base = unsafe { export::<PGET_MODULE_BASE_ROUTINE64>(module, c"SymGetModuleBase64") };
            let library = |set_options, initialize, cleanup, from_address, walk, table_access, module_base| Self {
                module,
                set_options,
                initialize,
                cleanup,
                from_address,
                walk,
                table_access,
                module_base,
            };
            match (set_options, initialize, cleanup, from_address, walk, table_access, module_base) {
                (Ok(a), Ok(b), Ok(c), Ok(d), Ok(e), Ok(f), Ok(g)) => Ok(library(a, b, c, d, e, f, g)),
                _ => {
                    // SAFETY: freeing the module we just loaded; nothing from it is kept.
                    unsafe { FreeLibrary(module) };
                    Err(Error::Malformed { call: "dbghelp exports" })
                }
            }
        }
    }

    /// Keeps a thread suspended for its lifetime.
    struct Suspended<'a>(&'a OwnedHandle);
    impl Drop for Suspended<'_> {
        fn drop(&mut self) {
            // SAFETY: the thread handle is live and this balances the successful SuspendThread.
            unsafe { ResumeThread(self.0.as_raw_handle()) };
        }
    }

    /// A dbghelp session on a process, cleaned up when dropped.
    struct Session<'a>(&'a Dbghelp, &'a OwnedHandle);
    impl Drop for Session<'_> {
        fn drop(&mut self) {
            // SAFETY: the session was initialized for this process handle, which is still live.
            unsafe { (self.0.cleanup)(self.1.as_raw_handle()) };
        }
    }

    pub(super) fn thread_stack(pid: u32, tid: u32) -> Result<Vec<StackFrame>> {
        let process = open_process(pid, PROCESS_QUERY_INFORMATION | PROCESS_VM_READ)?;
        let thread = open_thread(tid, THREAD_GET_CONTEXT | THREAD_SUSPEND_RESUME | THREAD_QUERY_INFORMATION)?;
        // SAFETY: `thread` is a live thread handle.
        if unsafe { GetProcessIdOfThread(thread.as_raw_handle()) } != pid {
            return Err(Error::ProcessGone);
        }
        let dbghelp = Dbghelp::load()?;
        // SAFETY: plain flags.
        unsafe {
            (dbghelp.set_options)(
                SYMOPT_UNDNAME
                    | SYMOPT_DEFERRED_LOADS
                    | SYMOPT_NO_PROMPTS
                    | SYMOPT_FAIL_CRITICAL_ERRORS
                    | SYMOPT_SECURE,
            )
        };
        let system32 =
            wide(&format!(r"{}\System32", std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into())));
        // SAFETY: `process` is live with query and read access; the search path is NUL-terminated;
        // "invade" makes dbghelp enumerate the process's loaded modules.
        if unsafe { (dbghelp.initialize)(process.as_raw_handle(), system32.as_ptr(), 1) } == 0 {
            return Err(Error::last_win32("SymInitializeW"));
        }
        let session = Session(&dbghelp, &process);

        let addresses = {
            // SAFETY: `thread` is live with THREAD_SUSPEND_RESUME.
            if unsafe { SuspendThread(thread.as_raw_handle()) } == u32::MAX {
                return Err(Error::last_win32("SuspendThread"));
            }
            let _suspended = Suspended(&thread);
            capture(&dbghelp, &process, &thread)?
        };

        let modules = modules(pid).unwrap_or_default();
        let frames = addresses
            .into_iter()
            .map(|address| {
                let symbol = match symbol_name(&dbghelp, &process, address) {
                    Some((name, offset)) => {
                        let module = symbolize(&modules, address);
                        let module = module.split('+').next().unwrap_or_default().to_owned();
                        format!("{module}!{name}+0x{offset:x}")
                    }
                    None => symbolize(&modules, address),
                };
                StackFrame { address, symbol }
            })
            .collect();
        drop(session);
        Ok(frames)
    }

    /// Return addresses, innermost first, while the thread is suspended.
    fn capture(dbghelp: &Dbghelp, process: &OwnedHandle, thread: &OwnedHandle) -> Result<Vec<u64>> {
        // SAFETY: CONTEXT is plain integers (and is 16-byte aligned by its type); all-zero is valid.
        let mut context: CONTEXT = unsafe { std::mem::zeroed() };
        context.ContextFlags = CONTEXT_FULL_AMD64;
        // SAFETY: `thread` is live with THREAD_GET_CONTEXT and suspended; `context` is aligned and writable.
        if unsafe { GetThreadContext(thread.as_raw_handle(), &mut context) } == 0 {
            return Err(Error::last_win32("GetThreadContext"));
        }
        let flat = |offset: u64| ADDRESS64 { Offset: offset, Segment: 0, Mode: AddrModeFlat };
        let mut frame = STACKFRAME64 {
            AddrPC: flat(context.Rip),
            AddrFrame: flat(context.Rbp),
            AddrStack: flat(context.Rsp),
            ..STACKFRAME64::default()
        };
        let mut addresses = Vec::new();
        while addresses.len() < MAX_FRAMES {
            // SAFETY: the handles are live, `frame` and `context` are valid and writable, the read
            // and translate routines are null (use defaults), and the other two are dbghelp's own.
            let ok = unsafe {
                (dbghelp.walk)(
                    IMAGE_FILE_MACHINE_AMD64,
                    process.as_raw_handle(),
                    thread.as_raw_handle(),
                    &mut frame,
                    (&raw mut context).cast(),
                    std::ptr::null(),
                    dbghelp.table_access,
                    dbghelp.module_base,
                    std::ptr::null(),
                )
            };
            let pc = frame.AddrPC.Offset;
            if ok == 0 || pc == 0 || addresses.last() == Some(&pc) {
                break;
            }
            addresses.push(pc);
        }
        Ok(addresses)
    }

    /// `(function name, offset into it)` from the module's exports or symbols.
    fn symbol_name(dbghelp: &Dbghelp, process: &OwnedHandle, address: u64) -> Option<(String, u64)> {
        let mut buffer = AlignedBuffer::new(size_of::<SYMBOL_INFOW>() + MAX_NAME as usize * 2);
        let header = SYMBOL_INFOW {
            SizeOfStruct: size_of::<SYMBOL_INFOW>() as u32,
            MaxNameLen: MAX_NAME,
            ..SYMBOL_INFOW::default()
        };
        // SAFETY: the buffer is 8-byte aligned (SYMBOL_INFOW's alignment) and larger than the header.
        unsafe { buffer.as_mut_ptr().cast::<SYMBOL_INFOW>().write(header) };
        let mut displacement = 0u64;
        // SAFETY: the session is initialized for `process`; the buffer holds a SYMBOL_INFOW with
        // room for MaxNameLen characters after it; `displacement` is a valid out-pointer.
        let ok = unsafe {
            (dbghelp.from_address)(process.as_raw_handle(), address, &mut displacement, buffer.as_mut_ptr().cast())
        };
        if ok == 0 {
            return None;
        }
        let bytes = buffer.bytes(buffer.capacity());
        let length = std::mem::offset_of!(SYMBOL_INFOW, NameLen);
        let len = u32::from_le_bytes(bytes.get(length..length + 4)?.try_into().ok()?).min(MAX_NAME) as usize;
        let start = std::mem::offset_of!(SYMBOL_INFOW, Name);
        let units: Vec<u16> = bytes
            .get(start..start + len * 2)?
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| u16::from_le_bytes(pair))
            .collect();
        Some((String::from_utf16_lossy(&units), displacement))
    }
}

#[cfg(not(target_arch = "x86_64"))]
mod walk {
    use super::StackFrame;
    use crate::{Error, Result};

    pub(super) fn thread_stack(_pid: u32, _tid: u32) -> Result<Vec<StackFrame>> {
        Err(Error::Malformed { call: "stack walking is implemented for x64 only" })
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;

    #[test]
    fn walks_a_waiting_child_process_stack() {
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "pause"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let pid = child.id();
        let threads = crate::threads::threads(pid).unwrap().unwrap();
        let stack = thread_stack(pid, threads[0].tid);
        let _ = child.kill();
        let _ = child.wait();
        let stack = stack.expect("stack");
        assert!(!stack.is_empty());
        assert!(stack.iter().any(|f| f.symbol.to_ascii_lowercase().starts_with("ntdll.dll!")), "{stack:#?}");
    }

    #[test]
    fn refuses_own_threads() {
        assert!(thread_stack(std::process::id(), 1).is_err());
    }
}
