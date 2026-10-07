//! Shared libraries loaded at run time (dlopen, LoadLibrary): a missing one
//! (no NVIDIA driver, no Opus) is a runtime error, never a link error.
//! Libraries stay loaded until the process exits.

use std::ffi::{CStr, c_char, c_void};
use std::mem::transmute_copy;

/// The library `name` (a file name, searched the system's usual way).
pub fn open(name: &CStr) -> Option<*mut c_void> {
    Some(unsafe { load(name.as_ptr()) }).filter(|h| !h.is_null())
}

/// `name` from `lib` as the function pointer type `F`.
pub unsafe fn sym<F>(lib: *mut c_void, name: &CStr) -> Option<F> {
    let p = unsafe { find(lib, name.as_ptr()) };
    (!p.is_null()).then(|| unsafe { transmute_copy::<*mut c_void, F>(&p) })
}

#[cfg(unix)]
unsafe fn load(name: *const c_char) -> *mut c_void {
    unsafe { libc::dlopen(name, libc::RTLD_NOW) }
}

#[cfg(unix)]
unsafe fn find(lib: *mut c_void, name: *const c_char) -> *mut c_void {
    unsafe { libc::dlsym(lib, name) }
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "LoadLibraryA"]
    fn load(name: *const c_char) -> *mut c_void;
    #[link_name = "GetProcAddress"]
    fn find(lib: *mut c_void, name: *const c_char) -> *mut c_void;
}
