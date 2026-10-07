//! macOS capture: CoreGraphics images of the main display, polled. There are
//! no damage events, so each `changed` takes a whole image and compares the
//! area with the last frame. Sizes are pixels, not points (Retina).
//! ponytail: CGDisplayCreateImage is deprecated (macOS 14) in favour of
//! ScreenCaptureKit, which also brings the pointer, other displays, single
//! windows and change notifications: the upgrade path, not done yet.
//!
//! The contract every backend keeps:
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
use crate::dylib::{self, sym};
use crate::frame::{Rect, Rows, Sprite, View, diff_rows, shows};
use std::ffi::c_void;
use std::time::Duration;

type CFTypeRef = *const c_void; // also CGImageRef, CGDataProviderRef, CFDataRef
type CreateImage = unsafe extern "C" fn(display: u32) -> CFTypeRef;
type Access = unsafe extern "C" fn() -> bool;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGImageGetWidth(image: CFTypeRef) -> usize;
    fn CGImageGetHeight(image: CFTypeRef) -> usize;
    fn CGImageGetBitsPerComponent(image: CFTypeRef) -> usize;
    fn CGImageGetBitsPerPixel(image: CFTypeRef) -> usize;
    fn CGImageGetBytesPerRow(image: CFTypeRef) -> usize;
    fn CGImageGetBitmapInfo(image: CFTypeRef) -> u32;
    fn CGImageGetDataProvider(image: CFTypeRef) -> CFTypeRef;
    fn CGDataProviderCopyData(provider: CFTypeRef) -> CFTypeRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8;
    fn CFDataGetLength(data: CFTypeRef) -> isize;
    fn CFRelease(cf: CFTypeRef);
}

// CGBitmapInfo: B,G,R,X in memory is 32-bit little-endian with alpha (or padding) first.
const ALPHA_MASK: u32 = 0x1F;
const PREMULTIPLIED_FIRST: u32 = 2;
const NONE_SKIP_FIRST: u32 = 6;
const ORDER_MASK: u32 = 0x7000;
const ORDER_32_LITTLE: u32 = 2 << 12;

/// A CoreFoundation object we own (a CGImage or CFData), released on drop.
struct Owned(CFTypeRef);

impl Owned {
    fn new(p: CFTypeRef) -> Option<Self> {
        (!p.is_null()).then_some(Owned(p))
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) };
    }
}

fn capture_failed() -> Box<dyn std::error::Error> {
    tr!("couldn't capture the screen", "no se pudo capturar la pantalla", "画面をキャプチャできませんでした").into()
}

pub struct Capture {
    /// Screen size, in pixels.
    pub sw: usize,
    pub sh: usize,
    /// The captured area: the whole screen unless `set_region` says otherwise.
    pub view: View,
    pub overlay: Option<Sprite>,
    pub overlay_seen: bool,
    #[allow(dead_code)] // ponytail: no pointer in CGDisplayCreateImage; drawing it needs NSCursor
    pub draw_pointer: bool,
    display: u32,
    create: CreateImage,
    buf: Box<[u8]>,    // what frame() shows: sw * sh * 4, never moved
    staging: Vec<u8>,  // the area as last captured, packed
    fresh: bool,       // `changed` just filled staging: `grab` copies from it
    full: bool,        // the next `changed` reports every row
    still: bool,       // the last `changed` found nothing: `wait` sleeps
}

impl Capture {
    pub fn new() -> Res<Self> {
        let unavailable = || tr!("screen capture isn't available on this system yet", "la captura de pantalla aún no está disponible en este sistema", "この環境ではまだ画面キャプチャを利用できません");
        let cg = dylib::open(c"/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics").ok_or_else(unavailable)?;
        // Looked up, not linked: Apple deprecated it, and a dropped symbol must stay a runtime error.
        let create: CreateImage = unsafe { sym(cg, c"CGDisplayCreateImage") }.ok_or_else(unavailable)?;
        // Screen Recording permission (macOS 11+; before that there is no way to ask).
        let preflight: Option<Access> = unsafe { sym(cg, c"CGPreflightScreenCaptureAccess") };
        let request: Option<Access> = unsafe { sym(cg, c"CGRequestScreenCaptureAccess") };
        if let (Some(preflight), Some(request)) = (preflight, request)
            && !unsafe { preflight() }
        {
            unsafe { request() }; // the system prompt; macOS shows it only once
            return Err(tr!(
                "screenrec may not record the screen yet: allow your terminal in System Settings › Privacy & Security › Screen Recording, then run it again",
                "screenrec aún no tiene permiso para grabar la pantalla: permite tu terminal en Configuración del Sistema › Privacidad y seguridad › Grabación de pantalla y vuelve a ejecutarlo",
                "screenrec にはまだ画面収録の許可がありません。システム設定 › プライバシーとセキュリティ › 画面収録 でターミナルを許可してから、もう一度実行してください"
            )
            .into());
        }
        let display = unsafe { CGMainDisplayID() };
        let img = Owned::new(unsafe { create(display) }).ok_or_else(capture_failed)?;
        let (sw, sh) = unsafe { (CGImageGetWidth(img.0), CGImageGetHeight(img.0)) };
        if sw == 0 || sh == 0 {
            return Err(capture_failed());
        }
        Ok(Capture {
            sw,
            sh,
            view: View { w: sw, h: sh, x0: 0, y0: 0 },
            overlay: None,
            overlay_seen: false,
            draw_pointer: true,
            display,
            create,
            buf: vec![0; sw * sh * 4].into_boxed_slice(),
            staging: vec![0; sw * sh * 4],
            fresh: false,
            full: true,
            still: false,
        })
    }

    /// Capture the display now and put the area's rows, packed, in `staging`.
    fn capture(&mut self) -> Res<()> {
        let img = Owned::new(unsafe { (self.create)(self.display) }).ok_or_else(capture_failed)?;
        let i = img.0;
        let (w, h) = unsafe { (CGImageGetWidth(i), CGImageGetHeight(i)) };
        if (w, h) != (self.sw, self.sh) {
            return Err(tr!(
                "the screen resolution changed ({}x{} -> {}x{}); start again",
                "la resolución de la pantalla cambió ({}x{} -> {}x{}); vuelve a empezar",
                "画面の解像度が変わりました ({}x{} -> {}x{})。もう一度やり直してください",
                self.sw, self.sh, w, h
            )
            .into());
        }
        let (bpp, bpc, info, stride) = unsafe { (CGImageGetBitsPerPixel(i), CGImageGetBitsPerComponent(i), CGImageGetBitmapInfo(i), CGImageGetBytesPerRow(i)) };
        let bgrx = bpp == 32 && bpc == 8 && matches!(info & ALPHA_MASK, PREMULTIPLIED_FIRST | NONE_SKIP_FIRST) && info & ORDER_MASK == ORDER_32_LITTLE;
        if !bgrx || stride < w * 4 {
            return Err(tr!(
                "unsupported screen pixel format (bitmap info {:#x}, {} bits per pixel)",
                "formato de píxel de pantalla no soportado (bitmap info {:#x}, {} bits por píxel)",
                "対応していない画面のピクセル形式です (bitmap info {:#x}、{} ビット/ピクセル)",
                info, bpp
            )
            .into());
        }
        let provider = unsafe { CGImageGetDataProvider(i) }; // owned by the image
        if provider.is_null() {
            return Err(capture_failed());
        }
        let data = Owned::new(unsafe { CGDataProviderCopyData(provider) }).ok_or_else(capture_failed)?;
        let (p, len) = unsafe { (CFDataGetBytePtr(data.0), CFDataGetLength(data.0)) };
        if p.is_null() || len < 0 || (len as usize) < (h - 1) * stride + w * 4 {
            return Err(capture_failed());
        }
        let src = unsafe { std::slice::from_raw_parts(p, len as usize) };
        let (v, row) = (self.view, self.view.w * 4);
        for (y, dst) in self.staging.chunks_exact_mut(row).enumerate() {
            let at = (v.y0 as usize + y) * stride + v.x0 as usize * 4;
            dst.copy_from_slice(&src[at..at + row]);
        }
        Ok(())
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
        self.staging.resize(self.view.w * self.view.h * 4, 0);
        self.fresh = false;
        self.invalidate();
    }

    /// The captured area, BGRX.
    pub fn frame(&self) -> &[u8] {
        &self.buf[..self.view.w * self.view.h * 4]
    }

    /// Refresh rows [y0, y1) of the area: from the capture `changed` just
    /// made, else from a new one.
    pub fn grab(&mut self, (y0, y1): Rows) -> Res<()> {
        if !std::mem::take(&mut self.fresh) {
            self.capture()?;
        }
        let row = self.view.w * 4;
        let r = y0 as usize * row..y1 as usize * row;
        self.buf[r.clone()].copy_from_slice(&self.staging[r]);
        Ok(())
    }

    /// Whether most opaque pixels of `s` are in the last grab, verbatim.
    pub fn shows(&self, s: &Sprite) -> bool {
        shows(self.frame(), self.view, s)
    }

    /// No pointer here (see `draw_pointer`): an empty sprite.
    pub fn query_cursor(&self) -> Res<(Sprite, u32)> {
        Ok((Sprite { x: 0, y: 0, w: 0, h: 0, argb: vec![] }, 0))
    }

    /// CGDisplayCreateImage already leaves the pointer out.
    pub fn hide_pointer(&self) -> Res<bool> {
        Ok(false)
    }

    pub fn show_pointer(&self) -> Res<()> {
        Ok(())
    }

    /// Nothing to set up: `changed` compares frames.
    pub fn track_changes(&mut self) -> Res<()> {
        Ok(())
    }

    pub fn follow_window(&mut self, _client: u32, _visible: Rect) -> Res<()> {
        Err(tr!("recording a single window isn't available on this system yet", "grabar una sola ventana aún no está disponible en este sistema", "この環境ではまだ単一ウィンドウを録画できません").into())
    }

    pub fn window_area(&self, _id: u32) -> Res<Rect> {
        Err(tr!("recording a single window isn't available on this system yet", "grabar una sola ventana aún no está disponible en este sistema", "この環境ではまだ単一ウィンドウを録画できません").into())
    }

    pub fn window_gone(&self) -> bool {
        false
    }

    /// No damage events: after a still screen, wait `timeout` before looking
    /// again; after a change, look again at once (record() already paces
    /// itself to the frame rate).
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

    /// Rows of the area (relative to it) that differ from the last frame:
    /// captures the display and keeps the capture for `grab`.
    pub fn changed(&mut self) -> Res<Option<Rows>> {
        self.capture()?;
        self.fresh = true;
        let rows = if std::mem::take(&mut self.full) { Some((0, self.view.h as i32)) } else { diff_rows(self.frame(), &self.staging, self.view.w * 4) };
        self.still = rows.is_none();
        Ok(rows)
    }
}
