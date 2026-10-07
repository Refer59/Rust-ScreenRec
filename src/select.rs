//! The frozen screen behind the launcher, as in GNOME: the screen as it was
//! when we started, dimmed except for the area being picked (a selection with
//! a border and corner handles, the window under the pointer, or all of it).
//! Screenshots are cut from this same image, so our UI is never in them.

use crate::Res;
use crate::capture::Capture;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as _;

pub use crate::frame::Rect;

const HANDLE: i32 = 11; // corner handle radius
const ACCENT: [u8; 3] = [0x4D, 0x7A, 0xFF]; // tangerine #FF7A4D, BGR

/// Border width in device pixels.
fn border(scale: f32) -> i32 {
    ((2.0 * scale).round() as i32).max(1)
}

fn union(a: Option<Rect>, b: Option<Rect>) -> Option<Rect> {
    match (a, b) {
        (Some(a), Some(b)) => Some((a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3))),
        (a, b) => a.or(b),
    }
}

pub fn contains(r: Rect, x: i32, y: i32) -> bool {
    x >= r.0 && x < r.2 && y >= r.1 && y < r.3
}

// X cursor font glyphs, see `cursors`.
pub const CURSOR_ARROW: usize = 0;
pub const CURSOR_CROSS: usize = 1;
const GLYPHS: [u16; 7] = [68, 34, 52, 134, 136, 12, 14]; // left_ptr crosshair fleur and the four corners

/// Fullscreen override-redirect window showing the frozen screen. Its
/// background pixmap is the canvas: we redraw into it, then ClearArea.
pub struct Overlay {
    pub win: u32,
    pix: u32,
    gc: u32,
    frozen: Vec<u8>,
    w: usize,
    h: usize,
    shown: (Option<Rect>, bool), // highlighted area, with handles?
    cursors: Vec<u32>,
    cursor: usize,
    /// UI scale (logical to device px); set between `new` and `show`.
    pub scale: f32,
}

impl Overlay {
    pub fn new(cap: &Capture, frozen: Vec<u8>, area: Option<Rect>, handles: bool) -> Res<Self> {
        let c = &cap.conn;
        let (w, h) = (cap.sw, cap.sh);
        let pix = c.generate_id()?;
        c.create_pixmap(24, pix, cap.root, w as u16, h as u16)?;
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
        let mut o = Overlay { win: 0, pix, gc, frozen, w, h, shown: (area, handles), cursors, cursor: CURSOR_ARROW, scale: 1.0 };
        o.win = c.generate_id()?;
        let events = EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION | EventMask::KEY_PRESS;
        let aux = CreateWindowAux::new().background_pixmap(pix).override_redirect(1).event_mask(events).cursor(o.cursors[0]);
        c.create_window(0, o.win, cap.root, 0, 0, w as u16, h as u16, 0, WindowClass::INPUT_OUTPUT, 0, &aux)?;
        c.change_property8(PropMode::REPLACE, o.win, AtomEnum::WM_NAME, AtomEnum::STRING, b"screenrec")?;
        Ok(o)
    }

    pub fn show(&self, conn: &impl Connection) -> Res<()> {
        self.put(conn, (0, 0, self.w as i32, self.h as i32))?;
        conn.map_window(self.win)?;
        Ok(())
    }

    /// The screen as it was at launch (BGRX, no pointer).
    pub fn frozen(&self) -> &[u8] {
        &self.frozen
    }

    /// Highlight `area` (None: dim everything), with corner handles or not.
    pub fn set(&mut self, conn: &impl Connection, area: Option<Rect>, handles: bool) -> Res<()> {
        if (area, handles) == self.shown {
            return Ok(());
        }
        let reach = |(a, hd): (Option<Rect>, bool)| {
            let m = if hd { (11.0 * self.scale).ceil() as i32 + 3 } else { border(self.scale) + 2 };
            a.map(|r| (r.0 - m, r.1 - m, r.2 + m, r.3 + m))
        };
        let dirty = union(reach(self.shown), reach((area, handles)));
        self.shown = (area, handles);
        let on_screen = |r: Rect| (r.0.max(0), r.1.max(0), r.2.min(self.w as i32), r.3.min(self.h as i32));
        if let Some(r) = dirty.map(on_screen).filter(|r| r.0 < r.2 && r.1 < r.3) {
            self.put(conn, r)?;
            conn.clear_area(false, self.win, r.0 as i16, r.1 as i16, (r.2 - r.0) as u16, (r.3 - r.1) as u16)?;
        }
        Ok(())
    }

    pub fn set_cursor(&mut self, conn: &impl Connection, which: usize) -> Res<()> {
        if which != self.cursor {
            self.cursor = which;
            conn.change_window_attributes(self.win, &ChangeWindowAttributesAux::new().cursor(self.cursors[which]))?;
        }
        Ok(())
    }

    /// Render `r` into the pixmap, in bands that keep requests small.
    fn put(&self, conn: &impl Connection, r: Rect) -> Res<()> {
        let rw = (r.2 - r.0) as usize;
        let band = ((2 << 20) / (rw * 4)).max(1) as i32;
        for y in (r.1..r.3).step_by(band as usize) {
            let y1 = (y + band).min(r.3);
            let data = self.render((r.0, y, r.2, y1));
            conn.put_image(ImageFormat::Z_PIXMAP, self.pix, self.gc, rw as u16, (y1 - y) as u16, r.0 as i16, y as i16, 0, 24, &data)?;
        }
        Ok(())
    }

    fn render(&self, (x0, y0, x1, y1): Rect) -> Vec<u8> {
        let (area, handles) = self.shown;
        let (rw, sc) = ((x1 - x0) as usize, self.scale);
        let bw = border(sc);
        let mut out = Vec::with_capacity(rw * (y1 - y0) as usize * 4);
        let dim = |v: u8| (v as u16 * 140 / 255) as u8;
        let edge = |v: u8| (dim(v) as u16 * 65 / 100) as u8; // black at 35% over the dim
        let grow = |a: Rect, m: i32| (a.0 - m, a.1 - m, a.2 + m, a.3 + m);
        for y in y0..y1 {
            let row = &self.frozen[(y as usize * self.w + x0 as usize) * 4..][..rw * 4];
            for (x, p) in (x0..).zip(row.as_chunks::<4>().0) {
                match area {
                    Some(a) if contains(a, x, y) => out.extend_from_slice(p),
                    Some(a) if contains(grow(a, bw), x, y) => out.extend_from_slice(&[ACCENT[0], ACCENT[1], ACCENT[2], 0]),
                    Some(a) if contains(grow(a, bw + 1), x, y) => out.extend_from_slice(&[edge(p[0]), edge(p[1]), edge(p[2]), 0]),
                    _ => out.extend_from_slice(&[dim(p[0]), dim(p[1]), dim(p[2]), 0]),
                }
            }
        }
        if let (Some(a), true) = (area, handles) {
            let reach = (11.0 * sc).ceil() as i32 + 3;
            let disc = |d: f32, r: f32, soft: f32| ((r + soft / 2.0 - d) / soft).clamp(0.0, 1.0);
            for (cx, cy) in [(a.0, a.1), (a.2, a.1), (a.0, a.3), (a.2, a.3)] {
                for y in (cy - reach).max(y0)..(cy + reach).min(y1) {
                    for x in (cx - reach).max(x0)..(cx + reach).min(x1) {
                        let d = ((x - cx) as f32 + 0.5).hypot((y - cy) as f32 + 0.5);
                        let (shadow, ring, fill) = (0.28 * disc(d, 10.5 * sc, 1.5), disc(d, 9.0 * sc, 1.0), disc(d, 6.5 * sc, 1.0));
                        let i = ((y - y0) as usize * rw + (x - x0) as usize) * 4;
                        for (v, acc) in out[i..i + 3].iter_mut().zip(ACCENT) {
                            let c = *v as f32 * (1.0 - shadow);
                            let c = c + (acc as f32 - c) * ring;
                            *v = (c + (255.0 - c) * fill).round() as u8;
                        }
                    }
                }
            }
        }
        out
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
    let o = Overlay { win: 0, pix: 0, gc: 0, frozen, w, h, shown: (area, handles), cursors: vec![], cursor: 0, scale: 1.25 };
    o.render((0, 0, w as i32, h as i32))
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

}
