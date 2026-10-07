//! Desktop integration on Windows: the same functions as the Linux desktop.rs.

use crate::Res;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::time::Duration;
use windows_sys::Win32::Foundation::GlobalFree;
use windows_sys::Win32::Graphics::Gdi::{BI_RGB, BITMAPINFOHEADER};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::System::Ole::{CF_DIB, CF_UNICODETEXT};
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

/// Put the w×h BGRX image (rows `stride` bytes apart) on the clipboard, as a
/// DIB (Windows makes the other bitmap formats from it). The system keeps a
/// copy, so it stays after we exit.
#[allow(dead_code)] // until shot copies (the clipboard + OCR settings)
pub fn copy_image(bgrx: &[u8], w: usize, h: usize, stride: usize) -> Res<()> {
    let head = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w as i32,
        biHeight: h as i32, // bottom-up, the form every reader takes
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        ..Default::default()
    };
    let mut dib = Vec::with_capacity(size_of::<BITMAPINFOHEADER>() + w * h * 4);
    dib.extend_from_slice(unsafe { std::slice::from_raw_parts((&raw const head).cast::<u8>(), size_of::<BITMAPINFOHEADER>()) });
    for y in (0..h).rev() {
        for p in bgrx[y * stride..][..w * 4].as_chunks::<4>().0 {
            dib.extend_from_slice(&[p[0], p[1], p[2], 255]); // opaque for readers that take the 4th byte as alpha
        }
    }
    set_clipboard(CF_DIB, &dib)
}

/// Put `text` on the clipboard (Windows line ends). The system keeps a copy,
/// so it stays after we exit.
#[allow(dead_code)] // until the OCR copies what it read
pub fn copy_text(text: &str) -> Res<()> {
    let wide: Vec<u16> = text.lines().collect::<Vec<_>>().join("\r\n").encode_utf16().chain([0]).collect();
    set_clipboard(CF_UNICODETEXT, unsafe { std::slice::from_raw_parts(wide.as_ptr().cast(), wide.len() * 2) })
}

/// Replace the clipboard's contents with `data` as `format`.
fn set_clipboard(format: u16, data: &[u8]) -> Res<()> {
    let failed = || tr!("couldn't copy to the clipboard", "no se pudo copiar al portapapeles", "クリップボードにコピーできませんでした");
    // Another program may have it open for a moment.
    let mut tries = 0;
    while unsafe { OpenClipboard(null_mut()) } == 0 {
        tries += 1;
        if tries == 20 {
            return Err(failed().into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let copied = unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE, data.len());
        let p = if mem.is_null() { null_mut() } else { GlobalLock(mem) };
        if !p.is_null() {
            std::ptr::copy_nonoverlapping(data.as_ptr(), p.cast(), data.len());
            GlobalUnlock(mem);
        }
        // From SetClipboardData on, the memory is the system's.
        let ok = !p.is_null() && EmptyClipboard() != 0 && !SetClipboardData(format.into(), mem).is_null();
        if !ok && !mem.is_null() {
            GlobalFree(mem);
        }
        CloseClipboard();
        ok
    };
    if copied { Ok(()) } else { Err(failed().into()) }
}
