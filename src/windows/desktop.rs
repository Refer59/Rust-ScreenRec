//! Desktop integration on Windows: the same functions as the Linux desktop.rs.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Threading::{BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, SetPriorityClass};
use windows_sys::Win32::UI::Shell::{FOLDERID_Pictures, FOLDERID_Videos, KF_FLAG_DEFAULT, SHGetKnownFolderPath};

/// How to install ffmpeg here, for error messages.
pub const GET_FFMPEG: &str = "winget install ffmpeg";

/// A console program has no notification of its own: the terminal gets it.
pub fn notify(title: &str, body: &str, _icon: Option<&Path>) {
    eprintln!("{title}: {body}");
}

/// The user's Pictures or Videos folder (wherever it was moved, e.g. to
/// OneDrive), else their profile folder.
pub fn user_dir(xdg_dir: &str) -> PathBuf {
    let id = if xdg_dir == "PICTURES" { &FOLDERID_Pictures } else { &FOLDERID_Videos };
    let mut p = null_mut();
    let hr = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT as u32, null_mut(), &mut p) };
    let known = (hr >= 0 && !p.is_null()).then(|| unsafe {
        let len = (0..).take_while(|&i| *p.add(i) != 0).count();
        PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(p, len)))
    });
    unsafe { CoTaskMemFree(p.cast()) }; // even on failure, says the documentation
    if let Some(dir) = known.filter(|d| d.is_dir()) {
        return dir;
    }
    let home = PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_else(|| ".".into()));
    let dir = home.join(if xdg_dir == "PICTURES" { "Pictures" } else { "Videos" });
    if dir.is_dir() { dir } else { home }
}

/// Unix time `secs` as local (year, month, day, hour, minute, second).
pub fn local_time(secs: u64) -> (i32, i32, i32, i32, i32, i32) {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_s(&mut tm, &(secs as libc::time_t)) };
    (tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// Whatever else runs comes first.
pub fn lower_priority() {
    unsafe { SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS) };
}
