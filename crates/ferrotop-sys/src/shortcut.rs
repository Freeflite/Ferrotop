//! Start menu shortcuts (`.lnk` files), written through the shell's `ShellLink` COM object: the
//! only supported way to make one.

use std::ffi::c_void;
use std::path::Path;

use windows_sys::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE};
use windows_sys::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows_sys::core::{GUID, HRESULT};

use crate::buffer::wide;
use crate::{Error, Result};

const CLSID_SHELL_LINK: GUID = GUID::from_u128(0x0002_1401_0000_0000_c000_0000_0000_0046);
const IID_SHELL_LINK_W: GUID = GUID::from_u128(0x0002_14f9_0000_0000_c000_0000_0000_0046);
const IID_PERSIST_FILE: GUID = GUID::from_u128(0x0000_010b_0000_0000_c000_0000_0000_0046);

type Method = unsafe extern "system" fn(*mut c_void) -> HRESULT;
type SetText = unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT;

#[repr(C)]
struct UnknownVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

/// `IShellLinkW`, in declaration order; only the setters used here are typed.
#[repr(C)]
struct ShellLinkVtbl {
    unknown: UnknownVtbl,
    get_path: Method,
    get_id_list: Method,
    set_id_list: Method,
    get_description: Method,
    set_description: SetText,
    get_working_directory: Method,
    set_working_directory: SetText,
    get_arguments: Method,
    set_arguments: SetText,
    get_hotkey: Method,
    set_hotkey: Method,
    get_show_cmd: Method,
    set_show_cmd: Method,
    get_icon_location: Method,
    set_icon_location: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> HRESULT,
    set_relative_path: Method,
    resolve: Method,
    set_path: SetText,
}

/// `IPersistFile` (after `IPersist::GetClassID`), in declaration order.
#[repr(C)]
struct PersistFileVtbl {
    unknown: UnknownVtbl,
    get_class_id: Method,
    is_dirty: Method,
    load: Method,
    save: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> HRESULT,
    save_completed: Method,
    get_cur_file: Method,
}

/// A COM interface pointer, released when dropped. `V` is its vtable's layout.
struct Interface<V> {
    pointer: *mut c_void,
    _vtable: std::marker::PhantomData<V>,
}

impl<V> Interface<V> {
    fn vtable(&self) -> &V {
        // SAFETY: a COM interface pointer points at a pointer to its vtable.
        let vtable = unsafe { *self.pointer.cast::<*const V>() };
        // SAFETY: the vtable lives as long as the object, and `V` is that interface's layout.
        unsafe { &*vtable }
    }

    fn unknown(&self) -> &UnknownVtbl {
        // SAFETY: as above; every COM vtable starts with IUnknown's three methods.
        let vtable = unsafe { *self.pointer.cast::<*const UnknownVtbl>() };
        // SAFETY: as above.
        unsafe { &*vtable }
    }
}

impl<V> Drop for Interface<V> {
    fn drop(&mut self) {
        let release = self.unknown().release;
        // SAFETY: this holds one reference to a live object, given back exactly once.
        unsafe { release(self.pointer) };
    }
}

fn check(hresult: HRESULT, call: &'static str) -> Result<()> {
    if hresult < 0 { Err(Error::HResult { call, hresult }) } else { Ok(()) }
}

/// COM for this thread while it's alive (if this call started it).
struct ComScope(bool);

impl ComScope {
    fn enter() -> Result<Self> {
        // SAFETY: a null reserved pointer is required; the matching uninitialize is in `drop`.
        let result = unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
        match result {
            // Already initialized on this thread, either way it was: use it as it is.
            RPC_E_CHANGED_MODE => Ok(ComScope(false)),
            S_FALSE => Ok(ComScope(true)),
            result => check(result, "CoInitializeEx").map(|()| ComScope(true)),
        }
    }
}

impl Drop for ComScope {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances the successful CoInitializeEx above, on the same thread.
            unsafe { CoUninitialize() };
        }
    }
}

/// Writes a shortcut at `link` that starts `target`, with its icon and `description`.
pub fn create(link: &Path, target: &Path, description: &str) -> Result<()> {
    let _com = ComScope::enter()?;
    let mut pointer = std::ptr::null_mut();
    // SAFETY: the CLSID and IID are the shell's documented ShellLink and IShellLinkW, and
    // `pointer` is a valid out pointer.
    let created = unsafe {
        CoCreateInstance(&CLSID_SHELL_LINK, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, &IID_SHELL_LINK_W, &mut pointer)
    };
    check(created, "CoCreateInstance(ShellLink)")?;
    let shell_link = Interface::<ShellLinkVtbl> { pointer, _vtable: std::marker::PhantomData };

    let (target_text, description_text) = (wide(&target.to_string_lossy()), wide(description));
    let folder = wide(&target.parent().unwrap_or(target).to_string_lossy());
    let vtable = shell_link.vtable();
    // SAFETY: each setter gets the live object and a NUL-terminated string it copies.
    check(unsafe { (vtable.set_path)(shell_link.pointer, target_text.as_ptr()) }, "IShellLinkW::SetPath")?;
    // SAFETY: as above.
    check(unsafe { (vtable.set_working_directory)(shell_link.pointer, folder.as_ptr()) }, "SetWorkingDirectory")?;
    // SAFETY: as above.
    check(unsafe { (vtable.set_description)(shell_link.pointer, description_text.as_ptr()) }, "SetDescription")?;
    // SAFETY: as above, with the icon's index in the file.
    check(unsafe { (vtable.set_icon_location)(shell_link.pointer, target_text.as_ptr(), 0) }, "SetIconLocation")?;

    let mut persist = std::ptr::null_mut();
    let query = shell_link.unknown().query_interface;
    // SAFETY: asks the live object for IPersistFile; `persist` is a valid out pointer.
    check(unsafe { query(shell_link.pointer, &IID_PERSIST_FILE, &mut persist) }, "QueryInterface(IPersistFile)")?;
    let persist_file = Interface::<PersistFileVtbl> { pointer: persist, _vtable: std::marker::PhantomData };
    let link_text = wide(&link.to_string_lossy());
    let save = persist_file.vtable().save;
    // SAFETY: saves to a NUL-terminated path; TRUE makes it the object's current file.
    check(unsafe { save(persist_file.pointer, link_text.as_ptr(), 1) }, "IPersistFile::Save")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_shortcut() {
        let dir = std::env::temp_dir().join(format!("ferrotop-shortcut-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let link = dir.join("Test.lnk");
        let target = std::env::current_exe().expect("exe");
        create(&link, &target, "A test shortcut").expect("shortcut");
        let bytes = std::fs::read(&link).expect("read");
        // The shell link header: its size, then the ShellLink CLSID.
        assert_eq!(bytes[..4], [0x4c, 0, 0, 0]);
        assert_eq!(bytes[4..8], [0x01, 0x14, 0x02, 0x00]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
