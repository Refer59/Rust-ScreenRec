//! X11 capture. MIT-SHM GetImage writes the screen straight into a shared
//! buffer (we never copy it), DAMAGE says which rows changed, XFIXES gives the
//! cursor, which GetImage leaves out unless the server uses a software cursor.
//!
//! X11 has no way to leave a window out of a capture, so the recording pill
//! is translucent and gets un-blended from every frame it shows up in.
//!
//! A single window is captured from its own Composite pixmap instead, so it
//! keeps recording while covered or moved (not while minimized: X has no
//! image of an unmapped window, the video holds its last frame).

use crate::Res;
use std::os::fd::AsRawFd;
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::composite::{ConnectionExt as _, Redirect};
use x11rb::protocol::damage::{ConnectionExt as _, ReportLevel};
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{ChangeWindowAttributesAux, ConnectionExt as _, CreateGCAux, EventMask, GrabMode, GrabStatus, ImageFormat, Rectangle};
use x11rb::rust_connection::RustConnection;
use x11rb::{CURRENT_TIME, NONE};

/// Half-open row range [y0, y1).
pub type Rows = (i32, i32);

fn union(a: Option<Rows>, b: Option<Rows>) -> Option<Rows> {
    match (a, b) {
        (Some(a), Some(b)) => Some((a.0.min(b.0), a.1.max(b.1))),
        (a, b) => a.or(b),
    }
}

fn overlaps(a: Rows, b: Rows) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// An image at a screen position, premultiplied ARGB: the cursor or one of our windows.
pub struct Sprite {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub argb: Vec<u32>,
}

impl Sprite {
    /// (pixel, screen x, screen y) of every non-transparent pixel, drawn at (x, y).
    fn pixels_at(&self, x: i32, y: i32) -> impl Iterator<Item = (u32, i32, i32)> + '_ {
        (0..self.h)
            .flat_map(move |sy| (0..self.w).map(move |sx| (self.argb[(sy * self.w + sx) as usize], x + sx, y + sy)))
            .filter(|(p, ..)| p >> 24 != 0)
    }

    fn covers(&self, x: i32, y: i32) -> bool {
        let (x, y) = (x - self.x, y - self.y);
        x >= 0 && y >= 0 && x < self.w && y < self.h && self.argb[(y * self.w + x) as usize] >> 24 != 0
    }
}

/// Where a BGRX frame sits on screen: w×h pixels with top-left at (x0, y0).
#[derive(Clone, Copy)]
pub struct View {
    pub w: usize,
    pub h: usize,
    pub x0: i32,
    pub y0: i32,
}

impl View {
    /// Byte offset of screen pixel (x, y), if it is in the frame.
    fn at(&self, x: i32, y: i32) -> Option<usize> {
        let (x, y) = (x - self.x0, y - self.y0);
        (x >= 0 && y >= 0 && (x as usize) < self.w && (y as usize) < self.h).then(|| (y as usize * self.w + x as usize) * 4)
    }

    /// Screen rows this frame covers.
    fn rows(&self) -> Rows {
        (self.y0, self.y0 + self.h as i32)
    }
}

/// Whether most opaque pixels of `s` are in the frame, verbatim.
fn shows(f: &[u8], v: View, s: &Sprite) -> bool {
    let (mut n, mut hit) = (0, 0);
    for (p, x, y) in s.pixels_at(s.x, s.y).filter(|(p, ..)| p >> 24 == 255) {
        if let Some(i) = v.at(x, y) {
            n += 1;
            hit += (f[i..i + 3] == p.to_le_bytes()[..3]) as usize;
        }
    }
    n > 0 && hit * 2 >= n
}

/// Alpha-blend `s` over the frame.
pub fn draw(f: &mut [u8], v: View, s: &Sprite) {
    for (p, x, y) in s.pixels_at(s.x, s.y) {
        if let Some(i) = v.at(x, y) {
            let a = p >> 24;
            for (k, d) in f[i..i + 3].iter_mut().enumerate() {
                *d = ((p >> (8 * k) & 255) + (*d as u32 * (255 - a) + 127) / 255) as u8;
            }
        }
    }
}

/// The compositor shows the pill `o`, drawn at (ox, oy), as o + under·(1 − a).
/// Solve for `under` in place, but only if every pixel fits that model first:
/// if the pill isn't on screen there as drawn (not composited yet, overview
/// open, covered) the frame is left alone: Some(false). None if no part of it
/// falls in the frame. Pixels under the cursor are skipped (on a
/// software-cursor server they hold the cursor, not the pill).
fn unblend(f: &mut [u8], v: View, o: &Sprite, (ox, oy): (i32, i32), cursor: Option<&Sprite>) -> Option<bool> {
    let mine = |x, y| !cursor.is_some_and(|c| c.covers(x, y));
    let (mut n, mut bad) = (0, 0);
    for (p, x, y) in o.pixels_at(ox, oy).filter(|&(p, x, y)| p >> 24 >= 32 && mine(x, y)) {
        let Some(i) = v.at(x, y) else { continue };
        let a = p >> 24;
        n += 1;
        bad += !(0..3).all(|k| {
            let (s, c) = (p >> (8 * k) & 255, f[i + k] as u32);
            c + 2 >= s && c <= s + (255 - a) + 2
        }) as usize;
    }
    if n == 0 {
        return None;
    }
    if bad * 100 > n {
        return Some(false);
    }
    for (p, x, y) in o.pixels_at(ox, oy).filter(|&(p, x, y)| p >> 24 < 255 && mine(x, y)) {
        let Some(i) = v.at(x, y) else { continue };
        let a = 255 - (p >> 24);
        for (k, d) in f[i..i + 3].iter_mut().enumerate() {
            *d = (((*d as u32).saturating_sub(p >> (8 * k) & 255) * 255 + a / 2) / a).min(255) as u8;
        }
    }
    Some(true)
}

/// The window being recorded, read from its own pixmap.
struct Follow {
    top: u32,         // its top-level window (a child of root): what gets redirected
    pix: u32,         // the top-level's contents; renamed whenever it is remapped or resized
    crop: (i32, i32), // the visible area's offset inside the top-level (frame, shadows)
    size: (i32, i32), // top-level size
    mapped: bool,
    gone: bool,
}

pub struct Capture {
    pub conn: RustConnection,
    pub root: u32,
    seg: u32,
    damage: u32,
    buf: *mut u8,
    /// Screen size.
    pub sw: usize,
    pub sh: usize,
    /// The captured area: the whole screen unless `set_region` says otherwise.
    pub view: View,
    cursor: Option<(Sprite, u32, bool)>, // with its XFIXES serial, and whether it is drawn
    pending: Option<Rows>,         // damaged screen rows not captured yet
    events: Vec<Event>,            // for our windows: clicks, exposes
    /// Our translucent pill; removed from every frame it is found in.
    pub overlay: Option<Sprite>,
    trail: Vec<(i32, i32)>, // where the pill just was: the compositor can be a frame behind a move
    /// Whether the last grab found the pill on screen (and removed it).
    pub overlay_seen: bool,
    /// Blend the cursor in when the server leaves it out of the image.
    pub draw_pointer: bool,
    follow: Option<Follow>,
}

impl Capture {
    pub fn new() -> Res<Self> {
        let (conn, screen) = x11rb::connect(None)?;
        let s = &conn.setup().roots[screen];
        if s.root_depth != 24 {
            return Err(tr!("color depth {} not supported (only 24)", "profundidad de color {} no soportada (solo 24)", "色深度 {} には対応していません (24 のみ)", s.root_depth).into());
        }
        let (root, sw, sh) = (s.root, s.width_in_pixels as usize, s.height_in_pixels as usize);
        conn.xfixes_query_version(5, 0)?.reply()?;
        let seg = conn.generate_id()?;
        let fd = conn.shm_create_segment(seg, (sw * sh * 4) as u32, false)?.reply()?.shm_fd;
        let buf = unsafe {
            libc::mmap(std::ptr::null_mut(), sw * sh * 4, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0)
        };
        if buf == libc::MAP_FAILED {
            return Err(tr!("mmap of the SHM segment failed", "mmap del segmento SHM falló", "SHM セグメントの mmap に失敗しました").into());
        }
        let view = View { w: sw, h: sh, x0: 0, y0: 0 };
        Ok(Capture {
            conn,
            root,
            seg,
            damage: 0,
            buf: buf.cast(),
            sw,
            sh,
            view,
            cursor: None,
            pending: None,
            events: vec![],
            overlay: None,
            trail: vec![],
            overlay_seen: false,
            draw_pointer: true,
            follow: None,
        })
    }

    /// Capture only this part of the screen from now on (clamped to it).
    pub fn set_region(&mut self, x: i32, y: i32, w: i32, h: i32) {
        let (x0, y0) = (x.clamp(0, self.sw as i32 - 1), y.clamp(0, self.sh as i32 - 1));
        let (x1, y1) = ((x + w).clamp(x0 + 1, self.sw as i32), (y + h).clamp(y0 + 1, self.sh as i32));
        self.view = View { w: (x1 - x0) as usize, h: (y1 - y0) as usize, x0, y0 };
        self.invalidate();
    }

    /// The captured area, BGRX.
    pub fn frame(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.buf, self.view.w * self.view.h * 4) }
    }

    /// Grab full-width rows of the area (relative to it). Same stride as the
    /// buffer, so the X server writes them exactly where they belong;
    /// untouched rows keep the previous frame.
    pub fn grab(&mut self, (y0, y1): Rows) -> Res<()> {
        let v = self.view;
        let all = unsafe { std::slice::from_raw_parts_mut(self.buf, v.w * v.h * 4) };
        let (src, sx, sy, w, h) = match &self.follow {
            None => (self.root, v.x0, v.y0 + y0, v.w as i32, y1 - y0),
            Some(f) => {
                let (sx, sy) = (f.crop.0, f.crop.1 + y0);
                (f.pix, sx, sy, (v.w as i32).min(f.size.0 - sx), (y1 - y0).min(f.size.1 - sy))
            }
        };
        if w == v.w as i32 && h > 0 {
            let offset = (y0 as usize * v.w * 4) as u32;
            let r = self.conn.shm_get_image(src, sx as i16, sy as i16, w as u16, h as u16, !0, ImageFormat::Z_PIXMAP.into(), self.seg, offset)?.reply();
            if r.is_err() && self.follow.is_some() {
                return Ok(()); // the window's pixmap went away mid-flight (unmapped, resized): keep the last frame
            }
            r?;
        } else if w > 0 && h > 0 {
            // Window narrower than when we started: its rows come back packed.
            let Ok(img) = self.conn.get_image(ImageFormat::Z_PIXMAP, src, sx as i16, sy as i16, w as u16, h as u16, !0)?.reply() else { return Ok(()) };
            for (i, row) in img.data.chunks_exact(w as usize * 4).enumerate() {
                let at = (y0 as usize + i) * v.w * 4;
                all[at..at + row.len()].copy_from_slice(row);
            }
        }
        // Only the rows just grabbed: everything else was already processed.
        let f = &mut all[y0 as usize * v.w * 4..y1 as usize * v.w * 4];
        let bv = View { w: v.w, h: (y1 - y0) as usize, x0: v.x0, y0: v.y0 + y0 };
        let cursor = self.cursor.as_ref().filter(|c| c.2).map(|c| &c.0);
        // `changed` makes a band touching the pill or the cursor cover all of
        // it: the pill is judged as a whole, the cursor blended exactly once.
        if let Some(o) = &self.overlay {
            let now = unblend(f, bv, o, (o.x, o.y), cursor);
            self.overlay_seen = now == Some(true)
                || self.trail.iter().any(|&p| unblend(f, bv, o, p, cursor) == Some(true))
                || now.is_none(); // not in this frame: nothing to remove
        }
        // A software cursor (e.g. on some PRIME laptops) is already in the image.
        if let Some(c) = cursor.filter(|c| self.draw_pointer && !shows(f, bv, c)) {
            draw(f, bv, c);
        }
        Ok(())
    }

    /// Whether most opaque pixels of `s` are in the last grab, verbatim.
    pub fn shows(&self, s: &Sprite) -> bool {
        shows(self.frame(), self.view, s)
    }

    /// Move the pill (already moved on screen); the old spot stays a candidate for a moment.
    pub fn move_overlay(&mut self, x: i32, y: i32) {
        if let Some(o) = self.overlay.as_mut().filter(|o| (o.x, o.y) != (x, y)) {
            self.trail.insert(0, (o.x, o.y));
            self.trail.truncate(3);
            (o.x, o.y) = (x, y);
        }
    }

    /// The pointer image as XFIXES reports it, with its serial.
    pub fn query_cursor(&self) -> Res<(Sprite, u32)> {
        let r = self.conn.xfixes_get_cursor_image()?.reply()?;
        let (x, y) = (r.x as i32 - r.xhot as i32, r.y as i32 - r.yhot as i32);
        Ok((Sprite { x, y, w: r.width as i32, h: r.height as i32, argb: r.cursor_image }, r.cursor_serial))
    }

    /// Grab the pointer with an invisible cursor (XFixesHideCursor doesn't
    /// take a software cursor out of the image; this does). False if another
    /// client holds a grab. Lasts until `show_pointer` or exit.
    pub fn hide_pointer(&self) -> Res<bool> {
        let c = &self.conn;
        let pix = c.generate_id()?;
        c.create_pixmap(1, pix, self.root, 1, 1)?;
        let gc = c.generate_id()?;
        c.create_gc(gc, pix, &CreateGCAux::new().foreground(0))?;
        c.poly_fill_rectangle(pix, gc, &[Rectangle { x: 0, y: 0, width: 1, height: 1 }])?;
        let blank = c.generate_id()?;
        c.create_cursor(blank, pix, pix, 0, 0, 0, 0, 0, 0, 0, 0)?;
        let r = c.grab_pointer(false, self.root, EventMask::NO_EVENT, GrabMode::ASYNC, GrabMode::ASYNC, NONE, blank, CURRENT_TIME)?;
        Ok(r.reply()?.status == GrabStatus::SUCCESS)
    }

    pub fn show_pointer(&self) -> Res<()> {
        self.conn.ungrab_pointer(CURRENT_TIME)?;
        Ok(())
    }

    /// Start tracking changes; only then does `changed` report anything.
    pub fn track_changes(&mut self) -> Res<()> {
        self.track(self.root)
    }

    fn track(&mut self, drawable: u32) -> Res<()> {
        self.conn.damage_query_version(1, 1)?.reply()?;
        self.damage = self.conn.generate_id()?;
        self.conn.damage_create(self.damage, drawable, ReportLevel::RAW_RECTANGLES)?;
        Ok(())
    }

    /// Record window `client` (showing at `visible`) from its own pixmap
    /// instead of the screen: covered or moved, it is still what's recorded.
    pub fn follow_window(&mut self, client: u32, (x0, y0, x1, y1): (i32, i32, i32, i32)) -> Res<()> {
        let c = &self.conn;
        c.composite_query_version(0, 4)?.reply()?;
        let mut top = client;
        loop {
            let parent = c.query_tree(top)?.reply()?.parent;
            if parent == self.root || parent == NONE {
                break;
            }
            top = parent;
        }
        // Fullscreen games ask the WM to stop compositing them, which would
        // leave no pixmap: keep it redirected ourselves (what OBS does too).
        c.composite_redirect_window(top, Redirect::AUTOMATIC)?;
        c.change_window_attributes(top, &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY))?;
        let g = c.get_geometry(top)?.reply()?;
        let at = c.translate_coordinates(top, self.root, 0, 0)?.reply()?;
        let crop = (x0 - at.dst_x as i32, y0 - at.dst_y as i32);
        let size = (g.width as i32, g.height as i32);
        self.view = View { w: (x1 - x0) as usize, h: (y1 - y0) as usize, x0, y0 };
        self.follow = Some(Follow { top, pix: 0, crop, size, mapped: true, gone: false });
        self.name_pixmap()?;
        self.track(top)?;
        self.invalidate();
        Ok(())
    }

    fn name_pixmap(&mut self) -> Res<()> {
        let Some(f) = self.follow.as_mut() else { return Ok(()) };
        if f.pix != 0 {
            self.conn.free_pixmap(f.pix)?;
        }
        f.pix = self.conn.generate_id()?;
        self.conn.composite_name_window_pixmap(f.top, f.pix)?;
        Ok(())
    }

    /// The followed window was closed.
    pub fn window_gone(&self) -> bool {
        self.follow.as_ref().is_some_and(|f| f.gone)
    }

    fn drain_events(&mut self) -> Res<()> {
        while let Some(ev) = self.conn.poll_for_event()? {
            match ev {
                Event::DamageNotify(e) => {
                    let (mut x, mut y) = (e.area.x as i32, e.area.y as i32);
                    let (v, w) = (self.view, e.area.width as i32);
                    if let Some(f) = &self.follow {
                        (x, y) = (x + v.x0 - f.crop.0, y + v.y0 - f.crop.1); // window -> screen
                    }
                    if x < v.x0 + v.w as i32 && v.x0 < x + w {
                        self.pending = union(self.pending, Some((y, y + e.area.height as i32)));
                    }
                }
                Event::ConfigureNotify(e) if self.follow.as_ref().is_some_and(|f| f.top == e.window) => {
                    let f = self.follow.as_mut().unwrap();
                    (self.view.x0, self.view.y0) = (e.x as i32 + f.crop.0, e.y as i32 + f.crop.1);
                    if (e.width as i32, e.height as i32) != f.size {
                        f.size = (e.width as i32, e.height as i32);
                        let v = self.view;
                        unsafe { std::ptr::write_bytes(self.buf, 0, v.w * v.h * 4) }; // what it no longer covers: black
                        self.name_pixmap()?;
                        self.invalidate();
                    }
                }
                Event::MapNotify(e) if self.follow.as_ref().is_some_and(|f| f.top == e.window) => {
                    self.follow.as_mut().unwrap().mapped = true;
                    self.name_pixmap()?;
                    self.invalidate();
                }
                Event::UnmapNotify(e) if self.follow.as_ref().is_some_and(|f| f.top == e.window) => {
                    self.follow.as_mut().unwrap().mapped = false;
                }
                Event::DestroyNotify(e) if self.follow.as_ref().is_some_and(|f| f.top == e.window) => {
                    self.follow.as_mut().unwrap().gone = true;
                }
                ev => self.events.push(ev),
            }
        }
        Ok(())
    }

    /// Events for our windows that arrived meanwhile.
    pub fn take_events(&mut self) -> Res<Vec<Event>> {
        self.drain_events()?;
        Ok(std::mem::take(&mut self.events))
    }

    /// Sleep until the screen changes, an event arrives, or `timeout` passes.
    pub fn wait(&mut self, timeout: Duration) -> Res<()> {
        self.conn.flush()?; // small requests (moves, repaints) sit in a buffer until then
        self.drain_events()?;
        if self.pending.is_none() && self.events.is_empty() {
            let mut fd = libc::pollfd { fd: self.conn.stream().as_raw_fd(), events: libc::POLLIN, revents: 0 };
            unsafe { libc::poll(&mut fd, 1, timeout.as_millis() as i32) };
            self.drain_events()?;
        }
        Ok(())
    }

    /// Make the next `changed` report the whole area.
    pub fn invalidate(&mut self) {
        self.pending = Some(self.view.rows());
    }

    /// Rows of the area (relative to it) that changed since the last call (the
    /// first call: all of them): damaged rows, plus the old and new cursor
    /// position when it moved or changed shape.
    pub fn changed(&mut self) -> Res<Option<Rows>> {
        self.drain_events()?;
        if self.follow.as_ref().is_some_and(|f| !f.mapped) {
            return Ok(None); // minimized: X has no image of it, the video holds the last frame
        }
        let mut rows = union(self.cursor.is_none().then_some(self.view.rows()), self.pending.take());
        if rows.is_some() {
            // Raw rectangles still pile up in the server-side region; keep it empty.
            self.conn.damage_subtract(self.damage, NONE, NONE)?;
        }
        let (c, serial) = self.query_cursor()?;
        // Following a window, the pointer is drawn only while it is over that window.
        let shown = match &self.follow {
            Some(f) => self.conn.query_pointer(self.root)?.reply()?.child == f.top,
            None => true,
        };
        let was = self.cursor.as_ref();
        if was.is_none_or(|(o, s, sh)| (o.x, o.y, *s, *sh) != (c.x, c.y, serial, shown)) {
            rows = union(rows, was.filter(|w| w.2).map(|(o, ..)| (o.y, o.y + o.h)));
            rows = union(rows, shown.then_some((c.y, c.y + c.h)));
        }
        self.cursor = Some((c, serial, shown));
        let mut spots = vec![];
        if let Some((c, _, true)) = &self.cursor {
            spots.push((c.y, c.y + c.h));
        }
        if let Some(o) = &self.overlay {
            spots.extend(std::iter::once(o.y).chain(self.trail.iter().map(|p| p.1)).map(|y| (y, y + o.h)));
        }
        for _ in 0..2 {
            for &s in &spots {
                if rows.is_some_and(|r| overlaps(r, s)) {
                    rows = union(rows, Some(s));
                }
            }
        }
        let (top, h) = (self.view.y0, self.view.h as i32);
        Ok(rows.map(|(a, b)| ((a - top).max(0), (b - top).min(h))).filter(|(a, b)| a < b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unblend_inverts_the_compositor() {
        // 1 px pill at a = 204: what the compositor shows over `under`, then back.
        let p = 0xCC_40_20_10u32;
        let pill = Sprite { x: 5, y: 7, w: 1, h: 1, argb: vec![p] };
        let v = View { w: 1, h: 1, x0: 5, y0: 7 };
        for under in [0u32, 37, 128, 200, 255] {
            let shown = |s: u32| (s as f32 + under as f32 * (1.0 - 204.0 / 255.0)).round() as u8;
            let mut f = [shown(0x10), shown(0x20), shown(0x40), 0];
            assert_eq!(unblend(&mut f, v, &pill, (5, 7), None), Some(true));
            for c in &f[..3] {
                assert!((*c as i32 - under as i32).abs() <= 3, "{under} -> {c}");
            }
        }
        // Pill not composited there (frame shows something else): left alone.
        let mut f = [250, 250, 250, 0];
        assert_eq!(unblend(&mut f, v, &pill, (5, 7), None), Some(false));
        assert_eq!(f[0], 250);
        // Pill outside the frame: nothing to do.
        assert_eq!(unblend(&mut f, v, &pill, (50, 70), None), None);
    }
}
