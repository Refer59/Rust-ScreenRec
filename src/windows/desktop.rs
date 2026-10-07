//! Desktop integration on Windows: the same functions as the Linux desktop.rs.

use std::ffi::c_void;
use std::path::{Path, PathBuf};

/// How to install ffmpeg here, for error messages.
pub const GET_FFMPEG: &str = "winget install ffmpeg";

/// A console program has no notification of its own: the terminal gets it.
pub fn notify(title: &str, body: &str, _icon: Option<&Path>) {
    eprintln!("{title}: {body}");
}

/// The user's Pictures or Videos folder, else their profile folder.
// ponytail: %USERPROFILE% layout; SHGetKnownFolderPath if folders are redirected (OneDrive).
pub fn user_dir(xdg_dir: &str) -> PathBuf {
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

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut c_void;
    fn SetPriorityClass(process: *mut c_void, class: u32) -> i32;
}

/// Whatever else runs comes first.
pub fn lower_priority() {
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x4000;
    unsafe { SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS) };
}
