//! The frozen screen behind the launcher, as in GNOME: the screen as it was
//! when we started, dimmed except for the area being picked (a selection with
//! viewfinder brackets at its corners, the window under the pointer, or all of it).
//! Screenshots are cut from this same image, so our UI is never in them.

use crate::Res;
use crate::capture::Capture;
use std::os::fd::AsRawFd;
use x11rb::connection::Connection;
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as _;

pub use crate::frame::Rect;

const HANDLE: i32 = 14; // a press this near a corner (plus 4 px) grabs it

/// Border width in device pixels: a hairline round a selection, 2 px round a window.
fn border(scale: f32, handles: bool) -> i32 {
    (((if handles { 1.0 } else { 2.0 }) * scale).round() as i32).max(1)
}

/// The corner brackets' thickness and arm length in device pixels.
fn bracket(scale: f32) -> (i32, i32) {
    (((3.0 * scale).round() as i32).max(2), (24.0 * scale).round() as i32)
}

fn union(a: Option<Rect>, b: Option<Rect>) -> Option<Rect> {
    match (a, b) {
        (Some(a), Some(b)) => Some((a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3))),
        (a, b) => a.or(b),
    }
}

fn dim(v: u8) -> u8 {
    (v as u16 * 140 / 255) as u8
}

/// `f` over BGRX pixels from `s` into `d`, X = 0. Two plain passes: both vectorize.
fn map_span(d: &mut [u8], s: &[u8], f: impl Fn(u8) -> u8) {
    for (d, &s) in d.iter_mut().zip(s) {
        *d = f(s);
    }
    for x in d.iter_mut().skip(3).step_by(4) {
        *x = 0;
    }
}

/// What can change when a highlight with margin `m` moves from `old` to
/// `new`: their grown union minus the core inside both shrunk by `m`, as up
/// to four non-overlapping bands (top, bottom, left, right).
fn dirty_bands(old: Rect, new: Rect, m: i32) -> Vec<Rect> {
    let u = (old.0.min(new.0) - m, old.1.min(new.1) - m, old.2.max(new.2) + m, old.3.max(new.3) + m);
    let c = (old.0.max(new.0) + m, old.1.max(new.1) + m, old.2.min(new.2) - m, old.3.min(new.3) - m);
    if c.0 >= c.2 || c.1 >= c.3 {
        return vec![u];
    }
    let bands = [(u.0, u.1, u.2, c.1), (u.0, c.3, u.2, u.3), (u.0, c.1, c.0, c.3), (c.2, c.1, u.2, c.3)];
    bands.into_iter().filter(|r| r.0 < r.2 && r.1 < r.3).collect()
}

pub fn contains(r: Rect, x: i32, y: i32) -> bool {
    x >= r.0 && x < r.2 && y >= r.1 && y < r.3
}

// X cursor font glyphs, see `cursors`.
pub const CURSOR_ARROW: usize = 0;
pub const CURSOR_CROSS: usize = 1;
const GLYPHS: [u16; 7] = [68, 34, 52, 134, 136, 12, 14]; // left_ptr crosshair fleur and the four corners

/// Fullscreen override-redirect window showing the frozen screen. Its
/// background pixmap is the canvas: we redraw into it, then ClearArea. The
/// frozen screen itself is the capture buffer, left alone from `freeze` until
/// a recording starts, so callers pass `cap.frame()` in rather than a copy.
pub struct Overlay {
    pub win: u32,
    pix: u32,
    gc: u32,
    w: usize,
    h: usize,
    shown: (Option<Rect>, bool), // highlighted area, with handles?
    cursors: Vec<u32>,
    cursor: usize,
    /// UI scale (logical to device px); set between `new` and `show`.
    pub scale: f32,
    /// An MIT-SHM segment laid out like the screen (w*h*4 bytes, BGRX): bands
    /// are rendered there and sent with ShmPutImage, which copies them into the
    /// pixmap without going through the socket. None: plain PutImage.
    shm: Option<(u32, *mut u8)>,
}

/// A shared segment the size of the screen, mapped here. None (or Err) when
/// the server can't hand out fd-backed segments.
fn shm_segment(cap: &Capture) -> Res<Option<(u32, *mut u8)>> {
    let c = &cap.conn;
    let len = cap.sw * cap.sh * 4;
    let seg = c.generate_id()?;
    let fd = c.shm_create_segment(seg, len as u32, false)?.reply()?.shm_fd;
    // SAFETY: a new shared mapping of the segment's fd, owned by the Overlay and unmapped in its Drop.
    let buf = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
    Ok((buf != libc::MAP_FAILED).then(|| (seg, buf.cast())))
}

impl Overlay {
    pub fn new(cap: &Capture, area: Option<Rect>, handles: bool) -> Res<Self> {
        let c = &cap.conn;
        let (w, h) = (cap.sw, cap.sh);
        let pix = c.generate_id()?;
        c.create_pixmap(24, pix, cap.root, w as u16, h as u16)?;
        let shm = shm_segment(cap).ok().flatten();
        let gc = c.generate_id()?;
        c.create_gc(gc, pix, &CreateGCAux::new())?;
        let font = c.generate_id()?;
        c.open_font(font, b"cursor")?;
        let mut cursors = vec![];
        for g in GLYPHS {
            let id = c.generate_id()?;
            c.create_glyph_cursor(id, font, font, g, g + 1, 0, 0, 0, 0xffff, 0xffff, 0xffff)?;
            cursors.push(id);
        }
        let mut o = Overlay { win: 0, pix, gc, w, h, shown: (area, handles), cursors, cursor: CURSOR_ARROW, scale: 1.0, shm };
        o.win = c.generate_id()?;
        let events = EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION | EventMask::KEY_PRESS;
        let aux = CreateWindowAux::new().background_pixmap(pix).override_redirect(1).event_mask(events).cursor(o.cursors[0]);
        c.create_window(0, o.win, cap.root, 0, 0, w as u16, h as u16, 0, WindowClass::INPUT_OUTPUT, 0, &aux)?;
        c.change_property8(PropMode::REPLACE, o.win, AtomEnum::WM_NAME, AtomEnum::STRING, b"screenrec")?;
        Ok(o)
    }

    pub fn show(&mut self, conn: &impl Connection, frozen: &[u8]) -> Res<()> {
        self.put(conn, frozen, (0, 0, self.w as i32, self.h as i32))?;
        conn.map_window(self.win)?;
        Ok(())
    }

    /// Take the overlay down for good: window, pixmap and shared segment go
    /// (a recording is about to start; the launcher needs none of it any more).
    pub fn release(&mut self, conn: &impl Connection) -> Res<()> {
        conn.unmap_window(self.win)?;
        conn.free_pixmap(self.pix)?;
        if let Some((seg, p)) = self.shm.take() {
            conn.shm_detach(seg)?;
            // SAFETY: `p` is our mapping of w*h*4 bytes and nothing uses it after this.
            unsafe { libc::munmap(p.cast(), self.w * self.h * 4) };
        }
        Ok(())
    }

    /// Highlight `area` (None: dim everything), with corner handles or not.
    pub fn set(&mut self, conn: &impl Connection, frozen: &[u8], area: Option<Rect>, handles: bool) -> Res<()> {
        if (area, handles) == self.shown {
            return Ok(());
        }
        let margin = |hd: bool| self.margin(hd);
        let reach = |(a, hd): (Option<Rect>, bool)| {
            let m = margin(hd);
            a.map(|r| (r.0 - m, r.1 - m, r.2 + m, r.3 + m))
        };
        let dirty = match (self.shown, (area, handles)) {
            ((Some(o), ho), (Some(n), hn)) if ho == hn => dirty_bands(o, n, margin(hn)),
            (old, new) => union(reach(old), reach(new)).into_iter().collect(),
        };
        self.shown = (area, handles);
        let (w, h) = (self.w as i32, self.h as i32);
        let on_screen = |r: Rect| (r.0.max(0), r.1.max(0), r.2.min(w), r.3.min(h));
        for r in dirty.into_iter().map(on_screen).filter(|r| r.0 < r.2 && r.1 < r.3) {
            self.put(conn, frozen, r)?;
            conn.clear_area(false, self.win, r.0 as i16, r.1 as i16, (r.2 - r.0) as u16, (r.3 - r.1) as u16)?;
        }
        Ok(())
    }

    /// How far an area's look reaches past its edges, in and out: the
    /// brackets' arms with handles, else the border and its dark edge.
    fn margin(&self, handles: bool) -> i32 {
        if handles { bracket(self.scale).1 } else { border(self.scale, false) + 1 }
    }

    pub fn set_cursor(&mut self, conn: &impl Connection, which: usize) -> Res<()> {
        if which != self.cursor {
            self.cursor = which;
            conn.change_window_attributes(self.win, &ChangeWindowAttributesAux::new().cursor(self.cursors[which]))?;
        }
        Ok(())
    }

    /// Render `r` into the pixmap: through the shared segment when there is one,
    /// else by PutImage in bands that keep requests small.
    fn put(&mut self, conn: &impl Connection, frozen: &[u8], r: Rect) -> Res<()> {
        let rw = (r.2 - r.0) as usize;
        if let Some((seg, p)) = self.shm {
            // SAFETY: `p` maps w*h*4 bytes for as long as `self` lives, and `&mut self`
            // makes this the only reference to it in the process while it lives. The
            // server reads the segment only while processing the ShmPutImage request,
            // which is written to the socket after these writes; a later band over the
            // same rows only ever makes a queued request copy newer pixels.
            let map = unsafe { std::slice::from_raw_parts_mut(p, self.w * self.h * 4) };
            self.render_into(frozen, r, &mut map[(r.1 as usize * self.w + r.0 as usize) * 4..], self.w * 4);
            let (w, h, rh) = (self.w as u16, self.h as u16, (r.3 - r.1) as u16);
            conn.shm_put_image(self.pix, self.gc, w, h, r.0 as u16, r.1 as u16, rw as u16, rh, r.0 as i16, r.1 as i16, 24, u8::from(ImageFormat::Z_PIXMAP), false, seg, 0)?;
            return Ok(());
        }
        let band = ((2 << 20) / (rw * 4)).max(1) as i32;
        for y in (r.1..r.3).step_by(band as usize) {
            let y1 = (y + band).min(r.3);
            let data = self.render(frozen, (r.0, y, r.2, y1));
            conn.put_image(ImageFormat::Z_PIXMAP, self.pix, self.gc, rw as u16, (y1 - y) as u16, r.0 as i16, y as i16, 0, 24, &data)?;
        }
        Ok(())
    }

    fn render(&self, frozen: &[u8], r: Rect) -> Vec<u8> {
        let rw = (r.2 - r.0) as usize * 4;
        let mut out = vec![0u8; rw * (r.3 - r.1) as usize];
        self.render_into(frozen, r, &mut out, rw);
        out
    }

    /// Render `r`, its row k at `out[k * stride..][..rw * 4]`.
    fn render_into(&self, frozen: &[u8], (x0, y0, x1, y1): Rect, out: &mut [u8], stride: usize) {
        let (area, handles) = self.shown;
        let rw = (x1 - x0) as usize;
        if rw == 0 || y1 <= y0 {
            return;
        }
        let sc = self.scale;
        let bw = border(sc, handles);
        let edge = |v: u8| (dim(v) as u16 * 65 / 100) as u8; // black at 35% over the dim
        let lift = if handles { 55 } else { 90 }; // the border: white at this % over the dim
        let white = |v: u8| (dim(v) as u16 + (255 - dim(v) as u16) * lift / 100) as u8;
        // Row by row, as spans: dim | edge | border | inside | border | edge | dim.
        for y in y0..y1 {
            let dst = &mut out[(y - y0) as usize * stride..][..rw * 4];
            let src = &frozen[(y as usize * self.w + x0 as usize) * 4..][..rw * 4];
            let (mut e0, mut b0, mut i0, mut i1, mut b1, mut e1) = (x1, x1, x1, x1, x1, x1); // all dim
            if let Some(a) = area.filter(|a| y >= a.1 - bw - 1 && y < a.3 + bw + 1) {
                let cl = |v: i32, lo: i32, hi: i32| v.clamp(lo, hi.max(lo));
                e0 = cl(a.0 - bw - 1, x0, x1);
                e1 = cl(a.2 + bw + 1, e0, x1);
                (b0, i0, i1, b1) = (e1, e1, e1, e1); // the edge's own row: edge only
                if y >= a.1 - bw && y < a.3 + bw {
                    b0 = cl(a.0 - bw, e0, e1);
                    b1 = cl(a.2 + bw, b0, e1);
                    (i0, i1) = (b1, b1); // a border row: no inside
                    if y >= a.1 && y < a.3 {
                        i0 = cl(a.0, b0, b1);
                        i1 = cl(a.2, i0, b1);
                    }
                }
            }
            let at = |x: i32| (x - x0) as usize * 4;
            let mut span = |from: i32, to: i32, f: &dyn Fn(u8) -> u8| map_span(&mut dst[at(from)..at(to)], &src[at(from)..at(to)], f);
            span(x0, e0, &dim);
            span(e0, b0, &edge);
            span(b0, i0, &white);
            span(i1, b1, &white);
            span(b1, e1, &edge);
            span(e1, x1, &dim);
            dst[at(i0)..at(i1)].copy_from_slice(&src[at(i0)..at(i1)]);
        }
        // Viewfinder brackets just outside the corners, nudged inside where the screen ends.
        if let (Some(a), true) = (area, handles) {
            let (t, l) = bracket(sc);
            let span = |c: i32, dir: i32, len: i32, lim: i32| match dir {
                1 => ((c - t).max(0), (c - t).max(0) + len),
                _ => ((c + t).min(lim) - len, (c + t).min(lim)),
            };
            let (w, h) = (self.w as i32, self.h as i32);
            for (cx, cy, dx, dy) in [(a.0, a.1, 1, 1), (a.2, a.1, -1, 1), (a.0, a.3, 1, -1), (a.2, a.3, -1, -1)] {
                let across = (span(cx, dx, l, w), span(cy, dy, t, h));
                let down = (span(cx, dx, t, w), span(cy, dy, l, h));
                for ((bx0, bx1), (by0, by1)) in [across, down] {
                    for y in by0.max(y0)..by1.min(y1) {
                        for x in bx0.max(x0)..bx1.min(x1) {
                            let i = (y - y0) as usize * stride + (x - x0) as usize * 4;
                            out[i..i + 3].fill(255);
                        }
                    }
                }
            }
        }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        if let Some((_, p)) = self.shm {
            // SAFETY: `p` is our mapping of w*h*4 bytes and nothing uses it after this.
            unsafe { libc::munmap(p.cast(), self.w * self.h * 4) };
        }
    }
}

/// EWMH queries about other clients' windows, with the atoms looked up once.
pub struct Ewmh {
    stacking: u32,
    current: u32,
    desktop: u32,
    state: u32,
    hidden: u32,
    kind: u32,
    kind_desktop: u32,
    kind_dock: u32,
    frame: u32,
    gtk_frame: u32,
    name: u32,
    pid: u32,
}

impl Ewmh {
    pub fn new(cap: &Capture) -> Self {
        let atom = |n: &str| cap.conn.intern_atom(false, n.as_bytes()).ok().and_then(|r| r.reply().ok()).map_or(0, |r| r.atom);
        Ewmh {
            stacking: atom("_NET_CLIENT_LIST_STACKING"),
            current: atom("_NET_CURRENT_DESKTOP"),
            desktop: atom("_NET_WM_DESKTOP"),
            state: atom("_NET_WM_STATE"),
            hidden: atom("_NET_WM_STATE_HIDDEN"),
            kind: atom("_NET_WM_WINDOW_TYPE"),
            kind_desktop: atom("_NET_WM_WINDOW_TYPE_DESKTOP"),
            kind_dock: atom("_NET_WM_WINDOW_TYPE_DOCK"),
            frame: atom("_NET_FRAME_EXTENTS"),
            gtk_frame: atom("_GTK_FRAME_EXTENTS"),
            name: atom("_NET_WM_NAME"),
            pid: atom("_NET_WM_PID"),
        }
    }

    fn prop(cap: &Capture, w: u32, a: u32) -> Vec<u32> {
        let r = cap.conn.get_property(false, w, a, AtomEnum::ANY, 0, 4096).ok().and_then(|r| r.reply().ok());
        r.and_then(|p| p.value32().map(|v| v.collect())).unwrap_or_default()
    }

    /// Where window `w` shows on screen, without shadows: its geometry plus
    /// the WM's frame (server-side decorations) minus GTK's (client-side shadows).
    pub fn visible(&self, cap: &Capture, w: u32) -> Option<Rect> {
        let g = cap.conn.get_geometry(w).ok()?.reply().ok()?;
        let t = cap.conn.translate_coordinates(w, cap.root, 0, 0).ok()?.reply().ok()?;
        let (x, y) = (t.dst_x as i32, t.dst_y as i32);
        let mut r = (x, y, x + g.width as i32, y + g.height as i32);
        if let [l, rr, tp, b] = Self::prop(cap, w, self.frame)[..] {
            r = (r.0 - l as i32, r.1 - tp as i32, r.2 + rr as i32, r.3 + b as i32);
        }
        if let [l, rr, tp, b] = Self::prop(cap, w, self.gtk_frame)[..] {
            r = (r.0 + l as i32, r.1 + tp as i32, r.2 - rr as i32, r.3 - b as i32);
        }
        let r = (r.0.max(0), r.1.max(0), r.2.min(cap.sw as i32), r.3.min(cap.sh as i32));
        (r.0 < r.2 && r.1 < r.3).then_some(r)
    }

    /// Visible windows of the current workspace, topmost first, with their ids.
    pub fn windows(&self, cap: &Capture) -> Vec<(Rect, u32)> {
        let now = Self::prop(cap, cap.root, self.current).first().copied();
        let mut out = vec![];
        for w in Self::prop(cap, cap.root, self.stacking).into_iter().rev() {
            let on_desktop = Self::prop(cap, w, self.desktop).first().is_none_or(|&d| Some(d) == now || d == u32::MAX);
            let hidden = Self::prop(cap, w, self.state).contains(&self.hidden);
            let shell = Self::prop(cap, w, self.kind).iter().any(|&k| k == self.kind_desktop || k == self.kind_dock);
            if let (true, false, false, Some(r)) = (on_desktop, hidden, shell, self.visible(cap, w)) {
                out.push((r, w));
            }
        }
        out
    }

    /// A short name for window `w`: its title, or the app in "document - App".
    pub fn name(&self, cap: &Capture, w: u32) -> Option<String> {
        let get = |a: u32| cap.conn.get_property(false, w, a, AtomEnum::ANY, 0, 1024).ok()?.reply().ok().filter(|p| !p.value.is_empty());
        let p = get(self.name).or_else(|| get(AtomEnum::WM_NAME.into()))?;
        let title = String::from_utf8_lossy(&p.value).trim().to_owned();
        let app = title.rsplit(" - ").next().unwrap_or(&title);
        let app = app.rsplit(" — ").next().unwrap_or(app).trim();
        Some(if app.is_empty() { title.clone() } else { app.to_owned() })
    }

    /// The process behind window `w` (_NET_WM_PID), to follow its sound.
    pub fn pid(&self, cap: &Capture, w: u32) -> Option<u32> {
        Self::prop(cap, w, self.pid).first().copied()
    }
}

/// What a press grabs: a corner handle (0 tl, 1 tr, 2 bl, 3 br), the selection, or a new one.
#[derive(Clone, Copy)]
pub enum Grip {
    New((i32, i32)),
    Move((i32, i32), Rect),
    Corner(usize, Rect),
}

pub fn grip(sel: Rect, x: i32, y: i32) -> Grip {
    let corners = [(sel.0, sel.1), (sel.2, sel.1), (sel.0, sel.3), (sel.2, sel.3)];
    if let Some(i) = corners.iter().position(|&(cx, cy)| (cx - x).pow(2) + (cy - y).pow(2) <= (HANDLE + 4).pow(2)) {
        Grip::Corner(i, sel)
    } else if contains(sel, x, y) {
        Grip::Move((x, y), sel)
    } else {
        Grip::New((x, y))
    }
}

/// Cursor to show over (x, y): crosshair, move, or the corner's own.
pub fn grip_cursor(sel: Rect, x: i32, y: i32) -> usize {
    match grip(sel, x, y) {
        Grip::New(_) => CURSOR_CROSS,
        Grip::Move(..) => 2,
        Grip::Corner(i, _) => 3 + i,
    }
}

/// The selection with `g` dragged to (x, y), kept on a sw×sh screen.
pub fn drag(g: Grip, x: i32, y: i32, (sw, sh): (i32, i32)) -> Rect {
    let span = |a: (i32, i32), b: (i32, i32)| (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1));
    let (x, y) = (x.clamp(0, sw), y.clamp(0, sh));
    match g {
        Grip::New(a) => span(a, (x, y)),
        Grip::Corner(i, r) => span([(r.2, r.3), (r.0, r.3), (r.2, r.1), (r.0, r.1)][i], (x, y)),
        Grip::Move((ax, ay), r) => {
            let (w, h) = (r.2 - r.0, r.3 - r.1);
            let (nx, ny) = ((r.0 + x - ax).clamp(0, sw - w), (r.1 + y - ay).clamp(0, sh - h));
            (nx, ny, nx + w, ny + h)
        }
    }
}

/// The overlay as it would look over `frozen` (BGRX), for offscreen previews.
#[cfg(test)]
pub fn preview(frozen: Vec<u8>, w: usize, h: usize, area: Option<Rect>, handles: bool) -> Vec<u8> {
    let o = Overlay { win: 0, pix: 0, gc: 0, w, h, shown: (area, handles), cursors: vec![], cursor: 0, scale: 1.25, shm: None };
    o.render(&frozen, (0, 0, w as i32, h as i32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_editing() {
        let sel = (100, 100, 300, 200);
        // Dragging the bottom-right handle past the top-left corner flips the rectangle.
        let g = grip(sel, 301, 199);
        assert!(matches!(g, Grip::Corner(3, _)));
        assert_eq!(drag(g, 50, 60, (1920, 1080)), (50, 60, 100, 100));
        // Moving keeps the size and stays on screen.
        let g = grip(sel, 200, 150);
        assert_eq!(drag(g, 1900, 150, (1920, 1080)), (1720, 100, 1920, 200));
        // Outside: a new selection from the press point.
        assert_eq!(drag(grip(sel, 10, 10), 40, 5, (1920, 1080)), (10, 5, 40, 10));
    }

    /// The per-pixel `render` this module used to have: the reference.
    fn render_ref(o: &Overlay, frozen: &[u8], (x0, y0, x1, y1): Rect) -> Vec<u8> {
        let (area, handles) = o.shown;
        let (rw, sc) = ((x1 - x0) as usize, o.scale);
        let bw = border(sc, handles);
        let mut out = Vec::with_capacity(rw * (y1 - y0) as usize * 4);
        let dim = |v: u8| (v as u16 * 140 / 255) as u8;
        let edge = |v: u8| (dim(v) as u16 * 65 / 100) as u8;
        let lift = if handles { 55 } else { 90 };
        let white = |v: u8| (dim(v) as u16 + (255 - dim(v) as u16) * lift / 100) as u8;
        let grow = |a: Rect, m: i32| (a.0 - m, a.1 - m, a.2 + m, a.3 + m);
        for y in y0..y1 {
            let row = &frozen[(y as usize * o.w + x0 as usize) * 4..][..rw * 4];
            for (x, p) in (x0..).zip(row.as_chunks::<4>().0) {
                match area {
                    Some(a) if contains(a, x, y) => out.extend_from_slice(p),
                    Some(a) if contains(grow(a, bw), x, y) => out.extend_from_slice(&[white(p[0]), white(p[1]), white(p[2]), 0]),
                    Some(a) if contains(grow(a, bw + 1), x, y) => out.extend_from_slice(&[edge(p[0]), edge(p[1]), edge(p[2]), 0]),
                    _ => out.extend_from_slice(&[dim(p[0]), dim(p[1]), dim(p[2]), 0]),
                }
            }
        }
        if let (Some(a), true) = (area, handles) {
            let (t, l) = bracket(sc);
            let span = |c: i32, dir: i32, len: i32, lim: i32| match dir {
                1 => ((c - t).max(0), (c - t).max(0) + len),
                _ => ((c + t).min(lim) - len, (c + t).min(lim)),
            };
            let (w, h) = (o.w as i32, o.h as i32);
            for (cx, cy, dx, dy) in [(a.0, a.1, 1, 1), (a.2, a.1, -1, 1), (a.0, a.3, 1, -1), (a.2, a.3, -1, -1)] {
                let across = (span(cx, dx, l, w), span(cy, dy, t, h));
                let down = (span(cx, dx, t, w), span(cy, dy, l, h));
                for ((bx0, bx1), (by0, by1)) in [across, down] {
                    for y in by0.max(y0)..by1.min(y1) {
                        for x in bx0.max(x0)..bx1.min(x1) {
                            let i = ((y - y0) as usize * rw + (x - x0) as usize) * 4;
                            out[i..i + 3].fill(255);
                        }
                    }
                }
            }
        }
        out
    }

    fn overlay(w: usize, h: usize, shown: (Option<Rect>, bool)) -> (Overlay, Vec<u8>) {
        let mut seed = 12345u32;
        let frozen = (0..w * h * 4)
            .map(|_| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                (seed >> 24) as u8
            })
            .collect();
        (Overlay { win: 0, pix: 0, gc: 0, w, h, shown, cursors: vec![], cursor: 0, scale: 1.0, shm: None }, frozen)
    }

    #[test]
    fn render_matches_reference() {
        let (w, h) = (64, 40);
        let areas = [
            None,
            Some((20, 12, 44, 30)),
            Some((-10, 10, 30, 30)),
            Some((40, 10, 80, 30)),
            Some((10, -5, 50, 20)),
            Some((10, 20, 50, 60)),
            Some((0, 0, 64, 40)),
            Some((1, 1, 63, 39)),
            Some((30, 18, 33, 21)),
            Some((30, 18, 30, 21)), // empty: border only
        ];
        let rects = [
            (0, 0, 64, 40),
            (0, 10, 64, 20),
            (5, 0, 50, 40),
            (31, 0, 32, 40),
            (0, 19, 64, 20),
            (17, 9, 23, 15),
            (42, 27, 47, 33),
            (19, 11, 21, 31),
            (30, 18, 31, 19),
        ];
        let (mut o, frozen) = overlay(w, h, (None, false));
        for (a, scale) in areas.into_iter().flat_map(|a| [(a, 1.0), (a, 1.25)]) {
            o.scale = scale;
            for handles in [false, true] {
                o.shown = (a, handles);
                for r in rects {
                    assert!(o.render(&frozen, r) == render_ref(&o, &frozen, r), "area {a:?} handles {handles} scale {scale} rect {r:?}");
                }
            }
        }
    }

    #[test]
    fn render_into_with_stride() {
        let (w, h) = (64, 40);
        let rects = [(0, 0, 64, 40), (5, 3, 50, 37), (31, 0, 32, 40), (17, 9, 23, 15), (42, 27, 64, 40), (0, 19, 64, 20), (30, 18, 30, 21)];
        let (mut o, frozen) = overlay(w, h, (None, false));
        for a in [None, Some((20, 12, 44, 30)), Some((-10, 10, 30, 30)), Some((30, 18, 33, 21))] {
            for handles in [false, true] {
                o.shown = (a, handles);
                for r in rects {
                    let fill: Vec<u8> = (0..w * h * 4).map(|i| (i * 7 % 251) as u8).collect();
                    let mut buf = fill.clone();
                    o.render_into(&frozen, r, &mut buf[(r.1 as usize * w + r.0 as usize) * 4..], w * 4);
                    let want = o.render(&frozen, r);
                    let rw = (r.2 - r.0) as usize * 4;
                    for y in 0..h as i32 {
                        for x in 0..w as i32 {
                            let i = (y as usize * w + x as usize) * 4;
                            let px = if contains(r, x, y) {
                                let j = (y - r.1) as usize * rw + (x - r.0) as usize * 4;
                                &want[j..j + 4]
                            } else {
                                &fill[i..i + 4]
                            };
                            assert!(buf[i..i + 4] == *px, "area {a:?} handles {handles} rect {r:?} at ({x},{y})");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn dirty_bands_cover_every_change() {
        let (w, h) = (200, 150);
        let pairs = [
            ((40, 30, 140, 100), (50, 35, 150, 105)),  // move
            ((40, 30, 140, 100), (40, 30, 160, 120)),  // resize by a corner
            ((40, 30, 140, 100), (30, 20, 150, 110)),  // grow
            ((30, 20, 150, 110), (40, 30, 140, 100)),  // shrink
            ((10, 10, 50, 40), (120, 90, 180, 140)),   // disjoint
            ((40, 30, 140, 100), (40, 30, 140, 100)),  // identical
            ((20, 15, 180, 135), (80, 60, 100, 80)),   // one containing the other
            ((-20, -10, 60, 50), (-15, -10, 70, 60)),  // partly off-screen
            ((60, 50, 70, 60), (61, 50, 71, 60)),      // too small for a core
        ];
        let (mut o, frozen) = overlay(w, h, (None, false));
        let full = (0, 0, w as i32, h as i32);
        for (old, new, scale) in pairs.into_iter().flat_map(|(a, b)| [(a, b, 1.0), (a, b, 1.25)]) {
            o.scale = scale;
            for handles in [false, true] {
                let m = o.margin(handles);
                let bands = dirty_bands(old, new, m);
                o.shown = (Some(old), handles);
                let mut pix = render_ref(&o, &frozen, full);
                let before = pix.clone();
                o.shown = (Some(new), handles);
                let after = render_ref(&o, &frozen, full);
                let u = (old.0.min(new.0) - m, old.1.min(new.1) - m, old.2.max(new.2) + m, old.3.max(new.3) + m);
                for (i, b) in bands.iter().enumerate() {
                    assert!(b.0 >= u.0 && b.1 >= u.1 && b.2 <= u.2 && b.3 <= u.3, "{b:?} outside {u:?}");
                    for c in &bands[i + 1..] {
                        assert!(b.2 <= c.0 || c.2 <= b.0 || b.3 <= c.1 || c.3 <= b.1, "{b:?} overlaps {c:?}");
                    }
                }
                for y in 0..h as i32 {
                    for x in 0..w as i32 {
                        let i = (y as usize * w + x as usize) * 4;
                        if before[i..i + 4] != after[i..i + 4] {
                            assert!(bands.iter().any(|&b| contains(b, x, y)), "({x},{y}) changed outside {bands:?} for {old:?} -> {new:?}");
                        }
                    }
                }
                // Painting the bands (clipped, as `set` does) leaves the full new frame.
                for b in &bands {
                    let r = (b.0.max(0), b.1.max(0), b.2.min(w as i32), b.3.min(h as i32));
                    if r.0 >= r.2 || r.1 >= r.3 {
                        continue;
                    }
                    let data = o.render(&frozen, r);
                    let rw = (r.2 - r.0) as usize * 4;
                    for (y, row) in (r.1..).zip(data.chunks_exact(rw)) {
                        let i = (y as usize * w + r.0 as usize) * 4;
                        pix[i..i + rw].copy_from_slice(row);
                    }
                }
                assert!(pix == after, "{old:?} -> {new:?} handles {handles}");
            }
        }
    }
}
