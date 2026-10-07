//! Desktop integration on macOS: the same functions as the Linux desktop.rs.

use std::path::{Path, PathBuf};

/// How to install ffmpeg here, for error messages.
pub const GET_FFMPEG: &str = "brew install ffmpeg";

/// A Notification Center banner (osascript gets the text as arguments, so
/// nothing needs escaping).
pub fn notify(title: &str, body: &str, _icon: Option<&Path>) {
    let script = ["on run argv", "display notification (item 2 of argv) with title (item 1 of argv)", "end run"];
    let mut cmd = std::process::Command::new("osascript");
    for line in script {
        cmd.args(["-e", line]);
    }
    let _ = cmd.args([title, body]).spawn();
}

/// The user's Pictures or Movies folder, else their home.
pub fn user_dir(xdg_dir: &str) -> PathBuf {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into()));
    let dir = home.join(if xdg_dir == "PICTURES" { "Pictures" } else { "Movies" });
    if dir.is_dir() { dir } else { home }
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
