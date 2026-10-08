//! Desktop integration on macOS: the same functions as the Linux desktop.rs.

use crate::Res;
use std::ffi::{CStr, CString, c_char, c_void};
use std::path::{Path, PathBuf};

type Id = *mut c_void;

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Id;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    static NSPasteboardTypePNG: Id;
    static NSPasteboardTypeString: Id;
}

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

/// What places us in our desktop session, for a service that acts for us: nothing here. A user
/// has one GUI session, and the service reaches the pasteboard and Notification Center from it
/// like we do.
pub fn session_env() -> Vec<(String, Option<String>)> {
    Vec::new()
}

/// Act in the desktop session `env` (a client's `session_env`): ours already.
pub fn act_in(_env: Vec<(String, Option<String>)>) {}

/// The user's Pictures or Movies folder, else their home.
pub fn user_dir(xdg_dir: &str) -> PathBuf {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into()));
    let dir = home.join(if xdg_dir == "PICTURES" { "Pictures" } else { "Movies" });
    if dir.is_dir() { dir } else { home }
}

/// Where we keep files we can always download again (the OCR's runtime and
/// models): ~/Library/Caches/screenrec.
#[allow(dead_code)] // until the OCR downloads into it
pub fn cache_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join("Library/Caches/screenrec")
}

/// Where we keep files installed for good (the text recognizer's runtime and
/// models, put there by `screenrec install`): ~/Library/Application Support/screenrec.
pub fn data_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join("Library/Application Support/screenrec")
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

/// Put the w×h BGRX image (rows `stride` bytes apart) on the clipboard, as a
/// PNG. The pasteboard server keeps a copy, so it stays after we exit.
#[allow(dead_code)] // until shot copies (the clipboard + OCR settings)
pub fn copy_image(bgrx: &[u8], w: usize, h: usize, stride: usize) -> Res<()> {
    let png = crate::frame::png(bgrx, w, h, stride)?;
    let data: unsafe extern "C" fn(Id, Id, *const c_void, usize) -> Id = msg();
    put(unsafe { NSPasteboardTypePNG }, c"setData:forType:", || unsafe { data(class(c"NSData"), sel(c"dataWithBytes:length:"), png.as_ptr().cast(), png.len()) })
}

/// Put `text` on the clipboard. The pasteboard server keeps a copy, so it
/// stays after we exit.
#[allow(dead_code)] // until the OCR copies what it read
pub fn copy_text(text: &str) -> Res<()> {
    let text = CString::new(text)?;
    let string: unsafe extern "C" fn(Id, Id, *const c_char) -> Id = msg();
    put(unsafe { NSPasteboardTypeString }, c"setString:forType:", || unsafe { string(class(c"NSString"), sel(c"stringWithUTF8String:"), text.as_ptr()) })
}

/// Replace the general pasteboard's contents with what `value` makes, as `ty`
/// (`setter`: the NSPasteboard method that takes that kind of value).
fn put(ty: Id, setter: &CStr, value: impl FnOnce() -> Id) -> Res<()> {
    let send: unsafe extern "C" fn(Id, Id) -> Id = msg();
    let set: unsafe extern "C" fn(Id, Id, Id, Id) -> i8 = msg(); // BOOL
    let ok = unsafe {
        let pool = objc_autoreleasePoolPush(); // what the class methods return is autoreleased
        let pb = send(class(c"NSPasteboard"), sel(c"generalPasteboard"));
        send(pb, sel(c"clearContents"));
        let v = value();
        let ok = !pb.is_null() && !v.is_null() && set(pb, sel(setter), v, ty) != 0;
        objc_autoreleasePoolPop(pool);
        ok
    };
    if ok { Ok(()) } else { Err(tr!("couldn't copy to the clipboard", "no se pudo copiar al portapapeles", "クリップボードにコピーできませんでした").into()) }
}

/// objc_msgSend as the method's exact prototype `F`, which arm64 requires.
fn msg<F>() -> F {
    unsafe { std::mem::transmute_copy(&(objc_msgSend as unsafe extern "C" fn())) }
}

fn class(name: &CStr) -> Id {
    unsafe { objc_getClass(name.as_ptr()) }
}

fn sel(name: &CStr) -> Id {
    unsafe { sel_registerName(name.as_ptr()) }
}
