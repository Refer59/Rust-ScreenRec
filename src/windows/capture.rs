//! Windows capture with GDI: BitBlt from the screen into a DIB section,
//! compared with the last frame to find the changed rows (no damage events
//! here). Works everywhere (RDP, VMs, hybrid-GPU laptops); DXGI Desktop
//! Duplication is the faster upgrade. The whole virtual screen (every
//! monitor) is captured, in physical pixels: (0, 0) is its top-left corner.
//!
//! A single window is captured with PrintWindow, so it keeps recording while
//! covered or moved (not while minimized: the video holds its last frame).
//!
//! The contract every backend keeps (see the X11 capture.rs for each method):
//! - `frame()` is the captured area in BGRX, `view.w * 4` bytes per row, and
//!   always starts at the same address: NVENC pins the whole-screen buffer once
//!   (Video::new runs before `set_region`), so it is allocated at screen size
//!   in `new` and never moved.
//! - `grab(rows)` refreshes rows [y0, y1) of the area; other rows keep the
//!   previous frame. With `draw_pointer`, the pointer is blended in.
//! - `changed()` returns the rows that changed since the last call (all of them
//!   the first time, or after `invalidate`), None if nothing did.
//! - `wait(timeout)` sleeps until something may have changed, at most `timeout`.
//! - Our own windows (none outside X11 yet) must not end up in frames: `overlay`
//!   and `overlay_seen` exist for X11's un-blending and stay None/false here.

use crate::Res;
use crate::frame::{Rect, Rows, Sprite, View, diff_rows, shows};
use std::ptr::null_mut;
use std::time::Duration;
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, GetDC, HBITMAP, HDC,
    HGDIOBJ, ReleaseDC, SRCCOPY, SelectObject,
};
use windows_sys::Win32::Storage::Xps::PrintWindow;
use windows_sys::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GA_ROOT, GetAncestor, GetCursorInfo, GetIconInfo, GetSystemMetrics, GetWindowRect, HCURSOR, ICONINFO, IsIconic, IsWindow,
    IsWindowVisible, PW_RENDERFULLCONTENT, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WindowFromPoint,
};

/// The screen's device context.
struct ScreenDc(HDC);

impl Drop for ScreenDc {
    fn drop(&mut self) {
        unsafe { ReleaseDC(null_mut(), self.0) };
    }
}

/// A w×h top-down 32-bpp DIB section selected into its own memory DC: rows
/// are exactly w * 4 bytes, BGRX like our frames.
struct Dib {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    w: i32,
    h: i32,
}

impl Dib {
    fn new(screen: HDC, w: i32, h: i32) -> Res<Self> {
        let fail = || tr!("could not create a {}×{} capture bitmap", "no se pudo crear un mapa de bits de captura de {}×{}", "{}×{} のキャプチャ用ビットマップを作成できませんでした", w, h);
        let dc = unsafe { CreateCompatibleDC(screen) };
        if dc.is_null() {
            return Err(fail().into());
        }
        let mut d = Dib { dc, bmp: null_mut(), old: null_mut(), bits: null_mut(), w, h };
        let mut bi: BITMAPINFO = unsafe { std::mem::zeroed() };
        bi.bmiHeader = BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..Default::default()
        };
        let mut bits = null_mut();
        d.bmp = unsafe { CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0) };
        if d.bmp.is_null() || bits.is_null() {
            return Err(fail().into()); // `d` drops: frees what exists
        }
        d.bits = bits.cast();
        d.old = unsafe { SelectObject(dc, d.bmp) };
        if d.old.is_null() {
            return Err(fail().into());
        }
        Ok(d)
    }

    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.bits, self.w as usize * self.h as usize * 4) }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            if !self.old.is_null() {
                SelectObject(self.dc, self.old);
            }
            if !self.bmp.is_null() {
                DeleteObject(self.bmp);
            }
            DeleteDC(self.dc);
        }
    }
}

/// The window being recorded, read with PrintWindow.
struct Follow {
    hwnd: HWND,
    crop: (i32, i32), // the visible area's offset inside the window rect (no invisible borders)
    size: (i32, i32), // window rect size
    pic: Dib,         // its top-left crop.0 + view.w × crop.1 + view.h: all the view needs
}

pub struct Capture {
    /// Screen size.
    pub sw: usize,
    pub sh: usize,
    /// The captured area: the whole screen unless `set_region` says otherwise.
    pub view: View,
    pub overlay: Option<Sprite>,
    pub overlay_seen: bool,
    /// Draw the pointer into frames while recording.
    pub draw_pointer: bool,
    origin: (i32, i32), // the virtual screen's top-left, in desktop coordinates
    screen: ScreenDc,
    buf: Vec<u8>,       // the frame: sw * sh * 4, never reallocated
    staging: Dib,       // the view as last captured, view.w × view.h
    fresh: bool,        // staging holds a capture `grab` hasn't used yet
    full: bool,         // the next `changed` reports every row
    still: bool,        // the last `changed` found nothing
    tracking: bool,     // recording: the pointer goes into frames
    hot: (HCURSOR, i32, i32), // last cursor seen, with its hotspot
    follow: Option<Follow>,
}

impl Capture {
    pub fn new() -> Res<Self> {
        // Physical pixels, not scaled ones. Fails if already set (by a
        // manifest) or before Windows 10 1703: nothing to do about either.
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let m = |i| unsafe { GetSystemMetrics(i) };
        let origin = (m(SM_XVIRTUALSCREEN), m(SM_YVIRTUALSCREEN));
        let (w, h) = (m(SM_CXVIRTUALSCREEN), m(SM_CYVIRTUALSCREEN));
        if w <= 0 || h <= 0 {
            return Err(tr!("no screen to capture", "no hay pantalla que capturar", "キャプチャする画面がありません").into());
        }
        let screen = ScreenDc(unsafe { GetDC(null_mut()) });
        if screen.0.is_null() {
            return Err(tr!("could not open the screen for capture", "no se pudo abrir la pantalla para capturarla", "キャプチャのために画面を開けませんでした").into());
        }
        let staging = Dib::new(screen.0, w, h)?;
        let (sw, sh) = (w as usize, h as usize);
        Ok(Capture {
            sw,
            sh,
            view: View { w: sw, h: sh, x0: 0, y0: 0 },
            overlay: None,
            overlay_seen: false,
            draw_pointer: true,
            origin,
            screen,
            buf: vec![0; sw * sh * 4],
            staging,
            fresh: false,
            full: true,
            still: false,
            tracking: false,
            hot: (null_mut(), 0, 0),
            follow: None,
        })
    }

    /// The whole screen can always be read here.
    pub fn screen_readable(&self) -> Res<()> {
        Ok(())
    }

    /// Capture only this part of the screen from now on (clamped to it).
    pub fn set_region(&mut self, x: i32, y: i32, w: i32, h: i32) {
        let (x0, y0) = (x.clamp(0, self.sw as i32 - 1), y.clamp(0, self.sh as i32 - 1));
        let (x1, y1) = ((x + w).clamp(x0 + 1, self.sw as i32), (y + h).clamp(y0 + 1, self.sh as i32));
        self.view = View { w: (x1 - x0) as usize, h: (y1 - y0) as usize, x0, y0 };
        self.fresh = false;
        self.invalidate();
    }

    /// The captured area, BGRX.
    pub fn frame(&self) -> &[u8] {
        &self.buf[..self.view.w * self.view.h * 4]
    }

    /// Refresh rows of the area (relative to it): from what `changed` just
    /// captured, else from the screen now, without the pointer (a screenshot).
    pub fn grab(&mut self, (y0, y1): Rows) -> Res<()> {
        if !std::mem::take(&mut self.fresh) && !self.capture((y0, y1), false)? {
            return Err(tr!("could not read the screen", "no se pudo leer la pantalla", "画面を読み取れませんでした").into());
        }
        let (a, b) = (y0 as usize * self.view.w * 4, y1 as usize * self.view.w * 4);
        self.buf[a..b].copy_from_slice(&self.staging.bytes()[a..b]);
        Ok(())
    }

    /// Whether most opaque pixels of `s` are in the last grab, verbatim.
    pub fn shows(&self, s: &Sprite) -> bool {
        shows(self.frame(), self.view, s)
    }

    /// Screenshots never hold the pointer here: nothing to report or hide.
    pub fn query_cursor(&self) -> Res<(Sprite, u32)> {
        Ok((Sprite { x: 0, y: 0, w: 0, h: 0, argb: vec![] }, 0))
    }

    pub fn hide_pointer(&self) -> Res<bool> {
        Ok(false)
    }

    pub fn show_pointer(&self) -> Res<()> {
        Ok(())
    }

    /// Recording from now on: frames get the pointer.
    pub fn track_changes(&mut self) -> Res<()> {
        self.tracking = true;
        Ok(())
    }

    /// Record window `client` (an HWND, showing at `visible`) with
    /// PrintWindow instead of the screen: covered or moved, it is still what's recorded.
    pub fn follow_window(&mut self, client: u32, (x0, y0, x1, y1): Rect) -> Res<()> {
        let hwnd = client as usize as HWND;
        let mut r = RECT::default();
        if unsafe { IsWindow(hwnd) == 0 || GetWindowRect(hwnd, &mut r) == 0 } {
            return Err(tr!("that window is gone", "esa ventana ya no existe", "そのウィンドウはもうありません").into());
        }
        let crop = ((x0 + self.origin.0 - r.left).max(0), (y0 + self.origin.1 - r.top).max(0));
        let (w, h) = (x1 - x0, y1 - y0);
        let pic = Dib::new(self.screen.0, crop.0 + w, crop.1 + h)?;
        self.view = View { w: w as usize, h: h as usize, x0, y0 };
        self.follow = Some(Follow { hwnd, crop, size: (r.right - r.left, r.bottom - r.top), pic });
        self.tracking = true;
        self.fresh = false;
        self.invalidate();
        Ok(())
    }

    /// Where window `id` (an HWND) shows on screen (frame included, shadows not).
    pub fn window_area(&self, id: u32) -> Res<Rect> {
        let hidden = || tr!("that window is not visible on screen", "esa ventana no se ve en pantalla", "そのウィンドウは画面に表示されていません").into();
        self.visible(id as usize as HWND).ok_or_else(hidden)
    }

    fn visible(&self, hwnd: HWND) -> Option<Rect> {
        let mut r = RECT::default();
        unsafe {
            if IsWindow(hwnd) == 0 || IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
                return None;
            }
            let dwm = DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS as u32, (&raw mut r).cast(), size_of::<RECT>() as u32);
            if dwm < 0 && GetWindowRect(hwnd, &mut r) == 0 {
                return None;
            }
        }
        let (x0, y0) = ((r.left - self.origin.0).max(0), (r.top - self.origin.1).max(0));
        let (x1, y1) = ((r.right - self.origin.0).min(self.sw as i32), (r.bottom - self.origin.1).min(self.sh as i32));
        (x0 < x1 && y0 < y1).then_some((x0, y0, x1, y1))
    }

    /// The followed window was closed.
    pub fn window_gone(&self) -> bool {
        self.follow.as_ref().is_some_and(|f| unsafe { IsWindow(f.hwnd) } == 0)
    }

    /// No damage events: after a capture that found nothing, sleep `timeout`
    /// (the caller already slept until the tick, so after a change don't).
    /// Takes a Duration or an Option of one (None: no limit, so one poll
    /// interval here, as nothing says when the screen changes).
    pub fn wait(&mut self, timeout: impl Into<Option<Duration>>) -> Res<()> {
        if self.still {
            std::thread::sleep(timeout.into().unwrap_or(Duration::from_millis(16)));
        }
        Ok(())
    }

    /// Make the next `changed` report the whole area.
    pub fn invalidate(&mut self) {
        self.full = true;
    }

    /// Capture the area (with the pointer, when recording) and return the rows
    /// (relative to it) that differ from the last frame; all of them the first time.
    pub fn changed(&mut self) -> Res<Option<Rows>> {
        let h = self.view.h as i32;
        let ok = self.capture((0, h), self.tracking && self.draw_pointer)?;
        let rows = match ok {
            false => None, // screen unreadable (secure desktop) or window minimized: hold the last frame
            true if std::mem::take(&mut self.full) => Some((0, h)),
            true => diff_rows(self.frame(), self.staging.bytes(), self.view.w * 4),
        };
        (self.fresh, self.still) = (ok, rows.is_none());
        Ok(rows)
    }

    /// Capture rows [y0, y1) of the area into `staging`. False if there was
    /// nothing to read (the followed window is minimized, the screen locked).
    fn capture(&mut self, (y0, y1): Rows, pointer: bool) -> Res<bool> {
        let (w, h) = (self.view.w as i32, self.view.h as i32);
        if (self.staging.w, self.staging.h) != (w, h) {
            self.staging = Dib::new(self.screen.0, w, h)?;
        }
        let dst = self.staging.dc;
        let ok = match &mut self.follow {
            None => {
                let (sx, sy) = (self.origin.0 + self.view.x0, self.origin.1 + self.view.y0 + y0);
                unsafe { BitBlt(dst, 0, y0, w, y1 - y0, self.screen.0, sx, sy, SRCCOPY | CAPTUREBLT) != 0 }
            }
            Some(f) => {
                let mut r = RECT::default();
                if unsafe { IsIconic(f.hwnd) != 0 || GetWindowRect(f.hwnd, &mut r) == 0 } {
                    return Ok(false);
                }
                let size = (r.right - r.left, r.bottom - r.top);
                if size != f.size {
                    f.size = size;
                    let p = &f.pic;
                    unsafe { std::ptr::write_bytes(p.bits, 0, p.w as usize * p.h as usize * 4) }; // what it no longer covers: black
                }
                (self.view.x0, self.view.y0) = (r.left - self.origin.0 + f.crop.0, r.top - self.origin.1 + f.crop.1);
                unsafe { PrintWindow(f.hwnd, f.pic.dc, PW_RENDERFULLCONTENT) != 0 && BitBlt(dst, 0, y0, w, y1 - y0, f.pic.dc, f.crop.0, f.crop.1 + y0, SRCCOPY) != 0 }
            }
        };
        if ok && pointer {
            self.draw_cursor();
        }
        unsafe { GdiFlush() }; // GDI may batch: finish before the bits are read
        Ok(ok)
    }

    /// Draw the pointer into `staging` where it is on screen (following a
    /// window, only while it is over that window). Best effort: on any
    /// failure the frame just has no pointer.
    fn draw_cursor(&mut self) {
        let mut ci = CURSORINFO { cbSize: size_of::<CURSORINFO>() as u32, ..Default::default() };
        if unsafe { GetCursorInfo(&mut ci) } == 0 || ci.flags & CURSOR_SHOWING == 0 || ci.hCursor.is_null() {
            return;
        }
        if let Some(f) = &self.follow
            && unsafe { GetAncestor(WindowFromPoint(ci.ptScreenPos), GA_ROOT) } != f.hwnd
        {
            return;
        }
        if self.hot.0 != ci.hCursor {
            let mut ii = ICONINFO::default();
            if unsafe { GetIconInfo(ci.hCursor, &mut ii) } == 0 {
                return;
            }
            for b in [ii.hbmMask, ii.hbmColor].into_iter().filter(|b| !b.is_null()) {
                unsafe { DeleteObject(b) };
            }
            self.hot = (ci.hCursor, ii.xHotspot as i32, ii.yHotspot as i32);
        }
        let x = ci.ptScreenPos.x - self.origin.0 - self.view.x0 - self.hot.1;
        let y = ci.ptScreenPos.y - self.origin.1 - self.view.y0 - self.hot.2;
        unsafe { DrawIconEx(self.staging.dc, x, y, ci.hCursor, 0, 0, 0, null_mut(), DI_NORMAL) };
    }
}
