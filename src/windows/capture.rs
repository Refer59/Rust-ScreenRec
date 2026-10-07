//! Windows capture: not written yet. It has the same API as the X11
//! capture.rs (see it for what each method means), so main.rs builds here,
//! and `Capture::new` says it can't capture.
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
use crate::frame::{Rect, Rows, Sprite, View};
use std::time::Duration;

#[allow(dead_code)] // never built until there is a backend
pub struct Capture {
    /// Screen size.
    pub sw: usize,
    pub sh: usize,
    /// The captured area: the whole screen unless `set_region` says otherwise.
    pub view: View,
    pub overlay: Option<Sprite>,
    pub overlay_seen: bool,
    pub draw_pointer: bool,
}

impl Capture {
    pub fn new() -> Res<Self> {
        Err(tr!("screen capture isn't available on this system yet", "la captura de pantalla aún no está disponible en este sistema", "この環境ではまだ画面キャプチャを利用できません").into())
    }

    pub fn set_region(&mut self, _x: i32, _y: i32, _w: i32, _h: i32) {}

    pub fn frame(&self) -> &[u8] {
        &[]
    }

    pub fn grab(&mut self, _rows: Rows) -> Res<()> {
        Ok(())
    }

    pub fn shows(&self, _s: &Sprite) -> bool {
        false
    }

    pub fn query_cursor(&self) -> Res<(Sprite, u32)> {
        Ok((Sprite { x: 0, y: 0, w: 0, h: 0, argb: vec![] }, 0))
    }

    pub fn hide_pointer(&self) -> Res<bool> {
        Ok(false)
    }

    pub fn show_pointer(&self) -> Res<()> {
        Ok(())
    }

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

    pub fn flush(&self) -> Res<()> {
        Ok(())
    }

    pub fn wait(&mut self, _timeout: Duration) -> Res<()> {
        Ok(())
    }

    pub fn invalidate(&mut self) {}

    pub fn changed(&mut self) -> Res<Option<Rows>> {
        Ok(None)
    }
}
