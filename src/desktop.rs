//! Desktop integration on Linux: notifications, the user's folders, local
//! time, process priority. windows/desktop.rs and macos/desktop.rs have the
//! same functions.

use std::path::{Path, PathBuf};

/// How to install ffmpeg here, for error messages.
pub const GET_FFMPEG: &str = "sudo apt install ffmpeg";

pub fn notify(title: &str, body: &str, icon: Option<&Path>) {
    let icon = icon.map_or("media-record".into(), |p| p.display().to_string());
    let _ = std::process::Command::new("notify-send").args(["-a", "screenrec", "-i", &icon, title, body]).spawn();
}

/// The user's folder for `xdg_dir` (PICTURES, VIDEOS), else their home.
pub fn user_dir(xdg_dir: &str) -> PathBuf {
    let dir = std::process::Command::new("xdg-user-dir")
        .arg(xdg_dir)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| ".".into()));
    PathBuf::from(dir)
}

/// Unix time `secs` as local (year, month, day, hour, minute, second).
pub fn local_time(secs: u64) -> (i32, i32, i32, i32, i32, i32) {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&(secs as libc::time_t), &mut tm) };
    (tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// Whatever else runs comes first.
pub fn lower_priority() {
    unsafe { libc::nice(10) };
}
