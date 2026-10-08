//! Shared libraries loaded at run time (dlopen, LoadLibrary): a missing one
//! (no NVIDIA driver, no Opus) is a runtime error, never a link error.
//! Libraries stay loaded until the process exits.

use std::ffi::{CStr, c_char, c_void};
use std::mem::transmute_copy;
use std::path::Path;

/// The library `name` (a file name, searched the system's usual way).
pub fn open(name: &CStr) -> Option<*mut c_void> {
    Some(unsafe { load(name.as_ptr()) }).filter(|h| !h.is_null())
}

/// The library at `path`, a file of ours (say, in the cache): never a
/// system copy of the same name, and fine with any characters in the path.
pub fn open_path(path: &Path) -> Option<*mut c_void> {
    Some(unsafe { load_path(path) }).filter(|h| !h.is_null())
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

#[cfg(unix)]
unsafe fn load_path(path: &Path) -> *mut c_void {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes()).map_or(std::ptr::null_mut(), |p| unsafe { libc::dlopen(p.as_ptr(), libc::RTLD_NOW) })
}

/// Wide, so a profile path like C:\Users\José loads; the DLLs it needs are
/// looked for in its own folder first.
#[cfg(windows)]
unsafe fn load_path(path: &Path) -> *mut c_void {
    use std::os::windows::ffi::OsStrExt;
    const LOAD_WITH_ALTERED_SEARCH_PATH: u32 = 8;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    unsafe { load_wide(wide.as_ptr(), std::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH) }
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "LoadLibraryA"]
    fn load(name: *const c_char) -> *mut c_void;
    #[link_name = "GetProcAddress"]
    fn find(lib: *mut c_void, name: *const c_char) -> *mut c_void;
    #[link_name = "LoadLibraryExW"]
    fn load_wide(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
}

#[cfg(test)]
mod tests {
    #[test]
    fn open_path_loads_a_system_library_by_its_full_path() {
        // The C library this process already uses, by the full path it was loaded from
        // (distributions and cross sysroots keep it in different folders).
        #[cfg(unix)]
        let lib = &{
            use std::os::unix::ffi::OsStrExt;
            let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
            assert!(unsafe { libc::dladdr(libc::malloc as *const std::ffi::c_void, &mut info) } != 0);
            std::path::PathBuf::from(std::ffi::OsStr::from_bytes(unsafe { std::ffi::CStr::from_ptr(info.dli_fname) }.to_bytes()))
        };
        #[cfg(windows)]
        let lib = &std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32").join("kernel32.dll");
        assert!(super::open_path(lib).is_some(), "{}", lib.display());
        assert!(super::open_path(&lib.with_file_name("no-such-library")).is_none());
    }
}
