//! The GUI, drawn here pixel by pixel (antialiased signed-distance shapes,
//! text via ab_glyph) into ARGB override-redirect windows: the launcher
//! panel, its settings popover, and the recording pill. Knowing every pixel
//! we put on screen is what lets capture.rs take the pill back out of the video.

use crate::Res;
use crate::capture::{Capture, Sprite};
use crate::select::Rect;
use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as _;

/// Premultiplied ARGB pixels. Drawing takes logical units: `scale` maps them to
/// device pixels, `origin` shifts them, `alpha` fades everything drawn.
pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
    scale: f32,
    alpha: f32,
    origin: (f32, f32),
}

type Rgba = (f32, f32, f32, f32);
const fn hex(v: u32) -> Rgba {
    ((v >> 16 & 255) as f32 / 255.0, (v >> 8 & 255) as f32 / 255.0, (v & 255) as f32 / 255.0, 1.0)
}
// A camera for the screen: a near-black body, white controls, and two colours
// with one job each. Text is at least 6:1 on every surface it sits on.
const WHITE: Rgba = hex(0xFFFFFF);
const BLACK: Rgba = hex(0x0C0C0D);
const WELL: Rgba = hex(0x1F1F22);
const WELL_HI: Rgba = hex(0x2C2C31);
const THUMB: Rgba = hex(0x3A3A40);
const TEXT: Rgba = hex(0xF4F4F5);
const TEXT2: Rgba = hex(0xA1A1A8);
const DISABLED: Rgba = hex(0x55555C);
/// The chosen mode and the keyboard focus, nothing else.
const YELLOW: Rgba = hex(0xFFD23F);
/// Video: the shutter and REC.
const RED: Rgba = hex(0xFF3B30);
const HAIRLINE: Rgba = (1.0, 1.0, 1.0, 0.07);
const DIVIDER: Rgba = (1.0, 1.0, 1.0, 0.07);

fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    let l = |x: f32, y: f32| x + (y - x) * t;
    (l(a.0, b.0), l(a.1, b.1), l(a.2, b.2), l(a.3, b.3))
}

fn fade(c: Rgba, a: f32) -> Rgba {
    (c.0, c.1, c.2, c.3 * a)
}

// Motion.

static REDUCED: AtomicBool = AtomicBool::new(false);

/// With reduced motion every tween jumps straight to its target.
pub fn set_reduced_motion(on: bool) {
    REDUCED.store(on, Relaxed);
}

/// A value easing towards a target: ease-out cubic, or ease-in-out cubic.
#[derive(Clone, Copy)]
pub struct Tween {
    from: f32,
    to: f32,
    t0: Instant,
    ms: f32,
    inout: bool,
}

impl Tween {
    pub fn new(v: f32) -> Self {
        Tween { from: v, to: v, t0: Instant::now(), ms: 0.0, inout: false }
    }

    fn io(v: f32) -> Self {
        Tween { inout: true, ..Self::new(v) }
    }

    /// Head for `to` over `ms`, from wherever the value is now.
    pub fn go(&mut self, to: f32, ms: f32) {
        if to == self.to {
            return;
        }
        self.from = self.get();
        (self.to, self.t0, self.ms) = (to, Instant::now(), if REDUCED.load(Relaxed) { 0.0 } else { ms });
    }

    pub fn get(&self) -> f32 {
        let t = if self.ms > 0.0 { (self.t0.elapsed().as_secs_f32() * 1000.0 / self.ms).min(1.0) } else { 1.0 };
        let e = match (self.inout, t < 0.5) {
            (false, _) => 1.0 - (1.0 - t).powi(3),
            (true, true) => 4.0 * t * t * t,
            (true, false) => 1.0 - (2.0 - 2.0 * t).powi(3) / 2.0,
        };
        self.from + (self.to - self.from) * e
    }

    pub fn busy(&self) -> bool {
        self.ms > 0.0 && self.t0.elapsed().as_secs_f32() * 1000.0 < self.ms
    }

    pub fn settle(&mut self) {
        (self.from, self.ms) = (self.to, 0.0);
    }
}

/// Hover as a cross-fade: the control left fades out while the one entered fades in.
struct Hov<H> {
    from: Option<H>,
    to: Option<H>,
    a0: (f32, f32), // their amounts when it started
    t: Tween,
}

impl<H: Copy + PartialEq> Hov<H> {
    fn new() -> Self {
        Hov { from: None, to: None, a0: (0.0, 0.0), t: Tween::new(1.0) }
    }

    fn set(&mut self, h: Option<H>) {
        if h != self.to {
            let a0 = (self.to.map_or(0.0, |o| self.amt(o)), h.map_or(0.0, |n| self.amt(n)));
            (self.from, self.to, self.a0, self.t) = (self.to, h, a0, Tween::new(0.0));
            self.t.go(1.0, 120.0);
        }
    }

    fn amt(&self, h: H) -> f32 {
        let t = self.t.get();
        match () {
            _ if self.to == Some(h) => self.a0.1 + (1.0 - self.a0.1) * t,
            _ if self.from == Some(h) => self.a0.0 * (1.0 - t),
            _ => 0.0,
        }
    }

    fn settle(&mut self, h: Option<H>) {
        (self.from, self.to) = (None, h);
        self.t.settle();
    }
}

fn appear(ms: f32) -> Tween {
    let mut t = Tween::new(0.0);
    t.go(1.0, ms);
    t
}

// Shapes.

/// A shape: its bounding box and signed distance (negative inside).
struct Shape<F: Fn(f32, f32) -> f32> {
    b: (f32, f32, f32, f32),
    d: F,
}

fn sd_rrect(x: f32, y: f32, (x0, y0, x1, y1): (f32, f32, f32, f32), r: f32) -> f32 {
    let qx = (x - (x0 + x1) / 2.0).abs() - (x1 - x0) / 2.0 + r;
    let qy = (y - (y0 + y1) / 2.0).abs() - (y1 - y0) / 2.0 + r;
    let (ox, oy) = (qx.max(0.0), qy.max(0.0));
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}

fn circle(cx: f32, cy: f32, r: f32) -> Shape<impl Fn(f32, f32) -> f32> {
    Shape { b: (cx - r, cy - r, cx + r, cy + r), d: move |x: f32, y: f32| ((x - cx) * (x - cx) + (y - cy) * (y - cy)).sqrt() - r }
}

fn rrect(x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> Shape<impl Fn(f32, f32) -> f32> {
    Shape { b: (x0, y0, x1, y1), d: move |x, y| sd_rrect(x, y, (x0, y0, x1, y1), r) }
}

/// Segment from (ax, ay) to (bx, by), `r` thick on each side.
fn line(ax: f32, ay: f32, bx: f32, by: f32, r: f32) -> Shape<impl Fn(f32, f32) -> f32> {
    let d = move |x: f32, y: f32| {
        let (dx, dy) = (bx - ax, by - ay);
        let t = (((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
        let (ex, ey) = (x - ax - t * dx, y - ay - t * dy);
        (ex * ex + ey * ey).sqrt() - r
    };
    Shape { b: (ax.min(bx) - r, ay.min(by) - r, ax.max(bx) + r, ay.max(by) + r), d }
}

/// The outline of `s`, `w` wide.
fn stroke<F: Fn(f32, f32) -> f32>(s: Shape<F>, w: f32) -> Shape<impl Fn(f32, f32) -> f32> {
    let (x0, y0, x1, y1) = s.b;
    Shape { b: (x0 - w, y0 - w, x1 + w, y1 + w), d: move |x, y| (s.d)(x, y).abs() - w / 2.0 }
}

/// A control's outline, one table for what is drawn, focused and hit.
#[derive(Clone, Copy)]
enum Geo {
    Rect(f32, f32, f32, f32, f32),
    Disc(f32, f32, f32),
}

impl Geo {
    fn shape(self) -> Shape<impl Fn(f32, f32) -> f32> {
        let b = match self {
            Geo::Rect(x0, y0, x1, y1, _) => (x0, y0, x1, y1),
            Geo::Disc(x, y, r) => (x - r, y - r, x + r, y + r),
        };
        Shape {
            b,
            d: move |x, y| match self {
                Geo::Rect(.., r) => sd_rrect(x, y, b, r),
                Geo::Disc(cx, cy, r) => (x - cx).hypot(y - cy) - r,
            },
        }
    }

    fn grow(self, m: f32) -> Geo {
        match self {
            Geo::Rect(x0, y0, x1, y1, r) => Geo::Rect(x0 - m, y0 - m, x1 + m, y1 + m, r + m),
            Geo::Disc(x, y, r) => Geo::Disc(x, y, r + m),
        }
    }

    fn has(self, x: f32, y: f32) -> bool {
        match self {
            Geo::Rect(x0, y0, x1, y1, _) => x >= x0 && x < x1 && y >= y0 && y < y1,
            Geo::Disc(cx, cy, r) => (x - cx).hypot(y - cy) <= r,
        }
    }
}

impl Canvas {
    /// A canvas of `w`×`h` logical px: ceil(w·scale) × ceil(h·scale) device pixels.
    pub fn new(w: usize, h: usize, scale: f32) -> Self {
        let dev = |v: usize| (v as f32 * scale).ceil() as usize;
        let (w, h) = (dev(w), dev(h));
        Canvas { w, h, px: vec![0; w * h], scale, alpha: 1.0, origin: (0.0, 0.0) }
    }

    /// Paint opaque `src` over pixel (x, y) at opacity `a`.
    fn blend(&mut self, x: i32, y: i32, src: u32, a: f32) {
        let a8 = (a.clamp(0.0, 1.0) * 256.0 + 0.5) as u32;
        if a8 == 0 || x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return;
        }
        let p = &mut self.px[y as usize * self.w + x as usize];
        if a8 >= 256 {
            *p = src;
            return;
        }
        // Two channels at a time: src·a + dst·(1 − a), in 8.8 fixed point.
        let (inv, m) = (256 - a8, 0x00FF_00FF);
        let rb = (((src & m) * a8 + 0x0080_0080) >> 8 & m) + (((*p & m) * inv) >> 8 & m);
        let ag = (((src >> 8 & m) * a8 + 0x0080_0080) >> 8 & m) + (((*p >> 8 & m) * inv) >> 8 & m);
        *p = ag << 8 | rb;
    }

    fn paint<F: Fn(f32, f32) -> f32>(&mut self, color: Rgba, s: Shape<F>) {
        self.fill(color, s, 0.0);
    }

    /// Soft-edged fill, for shadows: coverage runs from 0 at `blur` outside to 1 at `blur` inside.
    fn soft<F: Fn(f32, f32) -> f32>(&mut self, color: Rgba, s: Shape<F>, blur: f32) {
        self.fill(color, s, blur);
    }

    /// Fill `s`. Big shapes go by 8×8 blocks, then 4×4: the distance at a block's
    /// centre says whether it is all outside, all inside (a solid fill) or has an edge.
    fn fill<F: Fn(f32, f32) -> f32>(&mut self, color: Rgba, s: Shape<F>, blur: f32) {
        if color.3 * self.alpha <= 0.0 {
            return;
        }
        let (sc, (ox, oy)) = (self.scale, self.origin);
        let (x0, y0, x1, y1) = s.b;
        let lo = |v: f32, o: f32| ((v - blur + o) * sc).floor() as i32 - 1;
        let hi = |v: f32, o: f32| ((v + blur + o) * sc).ceil() as i32 + 1;
        let (bx0, by0, bx1, by1) = (lo(x0, ox), lo(y0, oy), hi(x1, ox), hi(y1, oy));
        let (bw, bh) = (bx1 - bx0, by1 - by0);
        let big = bw.max(bh) >= 48 && bw.min(bh) >= 32; // the gear's distance isn't exact: small stays per pixel
        let (cx0, cy0, cx1, cy1) = (bx0.max(0), by0.max(0), bx1.min(self.w as i32), by1.min(self.h as i32));
        let inv = 1.0 / sc;
        let dist = |x: f32, y: f32| (s.d)(x * inv - ox, y * inv - oy);
        let cov = |d: f32| {
            if blur > 0.0 {
                let t = ((blur - d) / (2.0 * blur)).clamp(0.0, 1.0);
                t * t * (3.0 - 2.0 * t)
            } else {
                0.5 - d * sc
            }
        };
        let (bl, opacity) = (blur * sc, color.3 * self.alpha);
        let solid = opacity >= 1.0;
        let packed = pack(color.0, color.1, color.2);
        // 0: all outside, 1: all inside, 2: an edge crosses; `half` is the block's half-diagonal.
        let kind = |x: i32, y: i32, half: f32| {
            let d = dist(x as f32, y as f32) * sc;
            if d > half + 0.75 + bl { 0 } else if d < -half - 0.75 - bl { 1 } else { 2 }
        };
        // Soft edges (shadows) are smooth: one distance per 2×2 pixels will do.
        let step = if blur > 0.0 { 2 } else { 1 };
        let run = |this: &mut Self, x0: i32, y0: i32, x1: i32, y1: i32| {
            for y in (y0..y1).step_by(step) {
                for x in (x0..x1).step_by(step) {
                    let c = cov(dist(x as f32 + step as f32 / 2.0, y as f32 + step as f32 / 2.0));
                    if c <= 0.0 {
                        continue;
                    }
                    for py in y..(y + step as i32).min(y1) {
                        for px in x..(x + step as i32).min(x1) {
                            this.blend(px, py, packed, c.clamp(0.0, 1.0) * opacity);
                        }
                    }
                }
            }
        };
        let solid_rect = |this: &mut Self, x0: i32, y0: i32, x1: i32, y1: i32| {
            for y in y0..y1 {
                this.px[y as usize * this.w + x0 as usize..y as usize * this.w + x1 as usize].fill(packed);
            }
        };
        if !big {
            return run(self, cx0, cy0, cx1, cy1);
        }
        for by in (cy0..cy1).step_by(8) {
            for bx in (cx0..cx1).step_by(8) {
                let (bx1, by1) = ((bx + 8).min(cx1), (by + 8).min(cy1));
                match kind(bx + 4, by + 4, 5.66) {
                    0 => continue,
                    1 if solid => {
                        solid_rect(self, bx, by, bx1, by1);
                        continue;
                    }
                    _ => {}
                }
                for sy in (by..by1).step_by(4) {
                    for sx in (bx..bx1).step_by(4) {
                        let (sx1, sy1) = ((sx + 4).min(bx1), (sy + 4).min(by1));
                        match kind(sx + 2, sy + 2, 2.83) {
                            0 => {}
                            1 if solid => solid_rect(self, sx, sy, sx1, sy1),
                            _ => run(self, sx, sy, sx1, sy1),
                        }
                    }
                }
            }
        }
    }

    /// Any polygon (concave too), 4×4 supersampled.
    fn poly(&mut self, color: Rgba, p: &[(f32, f32)]) {
        let (sc, (ox, oy), opacity, src) = (self.scale, self.origin, color.3 * self.alpha, pack(color.0, color.1, color.2));
        let p: Vec<(f32, f32)> = p.iter().map(|&(x, y)| ((x + ox) * sc, (y + oy) * sc)).collect();
        let inside = |x: f32, y: f32| {
            let mut odd = false;
            for i in 0..p.len() {
                let ((ax, ay), (bx, by)) = (p[i], p[(i + 1) % p.len()]);
                if (ay > y) != (by > y) && x < ax + (y - ay) * (bx - ax) / (by - ay) {
                    odd = !odd;
                }
            }
            odd
        };
        let x0 = p.iter().map(|q| q.0).fold(f32::MAX, f32::min) as i32;
        let x1 = p.iter().map(|q| q.0).fold(f32::MIN, f32::max) as i32;
        let y0 = p.iter().map(|q| q.1).fold(f32::MAX, f32::min) as i32;
        let y1 = p.iter().map(|q| q.1).fold(f32::MIN, f32::max) as i32;
        for y in y0..=y1 {
            for x in x0..=x1 {
                let n = (0..16).filter(|i| inside(x as f32 + (i % 4) as f32 / 4.0 + 0.125, y as f32 + (i / 4) as f32 / 4.0 + 0.125)).count();
                self.blend(x, y, src, n as f32 / 16.0 * opacity);
            }
        }
    }

    /// Width of `s` at em size `px` (logical).
    fn width(font: &FontVec, s: &str, px: f32) -> f32 {
        let sf = font.as_scaled(font.pt_to_px_scale(px * 0.75).unwrap_or(PxScale::from(px)));
        s.chars().map(|ch| sf.h_advance(font.glyph_id(ch))).sum()
    }

    /// `s` cut to at most `max` px wide at em size `px`, with an ellipsis.
    fn fit(font: &FontVec, s: &str, px: f32, max: f32) -> String {
        if Self::width(font, s, px) <= max {
            return s.into();
        }
        let mut t: String = s.into();
        while t.pop().is_some() {
            let cut = t.trim_end();
            if Self::width(font, &format!("{cut}…"), px) <= max {
                return format!("{cut}…");
            }
        }
        "…".into()
    }

    /// `s` on `baseline`, `px` em size; `align` 0 puts its left end at x, 0.5 its centre.
    #[allow(clippy::too_many_arguments)]
    fn text(&mut self, font: &FontVec, s: &str, px: f32, x: f32, baseline: f32, align: f32, color: Rgba) {
        let (opacity, src) = (color.3 * self.alpha, pack(color.0, color.1, color.2));
        if opacity <= 0.0 {
            return;
        }
        let sc = self.scale;
        let scale = font.pt_to_px_scale(px * sc * 0.75).unwrap_or(PxScale::from(px * sc));
        let sf = font.as_scaled(scale);
        let mut x = (x + self.origin.0) * sc - s.chars().map(|ch| sf.h_advance(font.glyph_id(ch))).sum::<f32>() * align;
        let baseline = ((baseline + self.origin.1) * sc).round();
        for ch in s.chars() {
            let id = font.glyph_id(ch);
            if let Some(g) = font.outline_glyph(id.with_scale_and_position(scale, ab_glyph::point(x, baseline))) {
                let b = g.px_bounds();
                g.draw(|gx, gy, cov| self.blend(b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32, src, cov * opacity));
            }
            x += sf.h_advance(id);
        }
    }

    /// Each letter's advance, plus `track` after all but the last; digits get cells
    /// as wide as '0' when `tabular`, so a changing number doesn't jitter.
    fn advances(font: &FontVec, s: &str, px: f32, track: f32, tabular: bool) -> Vec<f32> {
        let cell = Self::width(font, "0", px);
        let n = s.chars().count();
        let adv = |(i, ch): (usize, char)| {
            let w = if tabular && ch.is_ascii_digit() { cell } else { Self::width(font, ch.encode_utf8(&mut [0; 4]), px) };
            w + if i + 1 < n { track } else { 0.0 }
        };
        s.chars().enumerate().map(adv).collect()
    }

    /// `text`, laid out letter by letter by `advances`.
    #[allow(clippy::too_many_arguments)]
    fn spaced(&mut self, font: &FontVec, s: &str, px: f32, (x, baseline): (f32, f32), align: f32, (track, tabular): (f32, bool), color: Rgba) {
        let adv = Self::advances(font, s, px, track, tabular);
        let mut x = x - adv.iter().sum::<f32>() * align;
        for (ch, w) in s.chars().zip(adv) {
            self.text(font, ch.encode_utf8(&mut [0; 4]), px, x, baseline, 0.0, color);
            x += w;
        }
    }

    /// Draw with `f` around (cx, cy), `k` times the size.
    fn scaled(&mut self, (cx, cy): (f32, f32), k: f32, f: impl FnOnce(&mut Self)) {
        let (sc, o) = (self.scale, self.origin);
        (self.scale, self.origin) = (sc * k, ((cx + o.0) / k, (cy + o.1) / k));
        f(self);
        (self.scale, self.origin) = (sc, o);
    }
}

fn spaced_width(font: &FontVec, s: &str, px: f32, track: f32, tabular: bool) -> f32 {
    Canvas::advances(font, s, px, track, tabular).iter().sum()
}

/// Small labels set like a camera's: tracked capitals in Latin scripts,
/// Japanese as written with a little air. Returns (text, size, tracking).
fn caps(s: &str, px: f32) -> (String, f32, f32) {
    match crate::i18n::lang() {
        crate::i18n::Lang::Ja => (s.to_owned(), px + 1.0, 0.6),
        _ => (s.to_uppercase(), px, px * 0.1),
    }
}

fn pack(r: f32, g: f32, b: f32) -> u32 {
    let q = |v: f32| (v * 255.0 + 0.5) as u32;
    255 << 24 | q(r) << 16 | q(g) << 8 | q(b)
}

/// The desktop's UI font (Ubuntu here), or a Japanese one; without one the
/// panel just has no labels.
pub fn load_font(ja: bool) -> Option<FontVec> {
    fc_font(if ja { "sans-serif:lang=ja" } else { "Ubuntu" })
}

/// Its medium weight, for the mode words and titles in Latin scripts.
pub fn load_bold() -> Option<FontVec> {
    fc_font("Ubuntu:medium")
}

fn fc_font(name: &str) -> Option<FontVec> {
    let out = std::process::Command::new("fc-match").args(["-f", "%{file}", name]).output().ok()?;
    FontVec::try_from_vec(std::fs::read(String::from_utf8(out.stdout).ok()?).ok()?).ok()
}

// Icons, line art in GNOME's symbolic style. `bg` is what's behind them, for cut-outs.

fn icon_camera(c: &mut Canvas, cx: f32, cy: f32, col: Rgba, bg: Rgba) {
    c.paint(col, rrect(cx - 4.0, cy - 8.0, cx + 4.0, cy - 3.0, 1.5));
    c.paint(col, rrect(cx - 9.5, cy - 5.5, cx + 9.5, cy + 7.5, 2.5));
    c.paint(bg, circle(cx, cy + 1.0, 4.4));
    c.paint(col, circle(cx, cy + 1.0, 2.4));
}

fn icon_video(c: &mut Canvas, cx: f32, cy: f32, col: Rgba) {
    c.paint(col, rrect(cx - 10.0, cy - 6.0, cx + 3.0, cy + 6.0, 2.0));
    c.poly(col, &[(cx + 4.0, cy - 1.5), (cx + 10.0, cy - 5.5), (cx + 10.0, cy + 5.5), (cx + 4.0, cy + 1.5)]);
}

fn icon_gear(c: &mut Canvas, cx: f32, cy: f32, col: Rgba, bg: Rgba) {
    // Eight teeth: the radius steps between 8.5 and 11 around the circle.
    let d = move |x: f32, y: f32| {
        let (dx, dy) = (x - cx, y - cy);
        dx.hypot(dy) - (9.75 + 1.25 * ((dy.atan2(dx) * 8.0).cos() * 3.0).clamp(-1.0, 1.0))
    };
    c.paint(col, Shape { b: (cx - 12.0, cy - 12.0, cx + 12.0, cy + 12.0), d });
    c.paint(bg, circle(cx, cy, 4.2));
}

type ShadowKey = (usize, usize, u32, u32, u32, u32);

thread_local! {
    /// Rendered shadows (alpha only), by canvas size, card and scale: they never change, so frames just copy them.
    static SHADOWS: std::cell::RefCell<Vec<(ShadowKey, Vec<u8>)>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Settled cards (shadow, body and hairline, opaque and at rest), by canvas size, card
    /// and scale: every frame after the appear animation starts from a copy of one.
    static CARDS: std::cell::RefCell<Vec<(CardKey, Vec<u32>)>> = const { std::cell::RefCell::new(Vec::new()) };
}
type CardKey = (usize, usize, u32, u32, u32, u32);

/// The soft shadow under a `w`×`h` body of radius `r`, drawn first on a still empty canvas.
fn shadow(c: &mut Canvas, w_: f32, h_: f32, r: f32) {
    let (w, h) = (w_, h_);
    let key = (c.w, c.h, w.to_bits(), h.to_bits(), r.to_bits(), c.scale.to_bits());
    SHADOWS.with_borrow_mut(|cache| {
        if !cache.iter().any(|e| e.0 == key) {
            let mut t = Canvas { w: c.w, h: c.h, px: vec![0; c.w * c.h], scale: c.scale, alpha: 1.0, origin: (M, M) };
            // The body hides the shadow's core: only its fringe is worth computing.
            let d = move |x, y| if sd_rrect(x, y, (0.0, 0.0, w, h), r) < -8.0 { f32::MAX } else { sd_rrect(x, y, (0.0, 6.0, w, h + 6.0), r) };
            t.soft((0.0, 0.0, 0.0, 0.45), Shape { b: (0.0, 6.0, w, h + 6.0), d }, 18.0);
            cache.push((key, t.px.iter().map(|p| (p >> 24) as u8).collect()));
        }
        let layer = &cache.iter().find(|e| e.0 == key).unwrap().1;
        let sh = |v: f32| (v * c.scale).round() as i32;
        let (dx, dy, a) = (sh(c.origin.0 - M), sh(c.origin.1 - M), (c.alpha * 256.0) as u32);
        let (w, h) = (c.w as i32, c.h as i32);
        let (x0, x1) = (dx.max(0), (w + dx).min(w));
        // The layer is empty under the body, and so is the canvas: skip that middle.
        let (ca, cb) = (sh(M + 8.0) + 2, sh(M + w_ - 8.0) - 2);
        let (ra, rb) = (sh(M + r), sh(M + h_ - r));
        for y in dy.max(0)..(h + dy).min(h) {
            let row = |c: &mut Canvas, xa: i32, xb: i32| {
                for x in xa.max(x0)..xb.min(x1) {
                    c.px[(y * w + x) as usize] = ((layer[((y - dy) * w + x - dx) as usize] as u32 * a) >> 8) << 24;
                }
            };
            if (ra..rb).contains(&(y - dy)) {
                row(c, 0, ca + dx);
                row(c, cb + dx, w);
            } else {
                row(c, 0, w);
            }
        }
    });
}

/// The one look of both cards: soft shadow, body, 1 px hairline inside its edge.
/// It is the first thing painted on an empty canvas; once settled (opaque, at
/// rest) its pixels are kept, and the next frame copies them instead.
fn card(c: &mut Canvas, w: f32, h: f32, r: f32) {
    let settled = c.alpha >= 1.0 && c.origin == (M, M);
    let key = (c.w, c.h, w.to_bits(), h.to_bits(), r.to_bits(), c.scale.to_bits());
    if settled && CARDS.with_borrow(|cache| cache.iter().find(|e| e.0 == key).map(|e| c.px.copy_from_slice(&e.1)).is_some()) {
        return;
    }
    shadow(c, w, h, r);
    c.paint(BLACK, rrect(0.0, 0.0, w, h, r));
    c.paint(HAIRLINE, stroke(rrect(0.5, 0.5, w - 0.5, h - 0.5, r - 0.5), 1.0));
    if settled {
        CARDS.with_borrow_mut(|cache| cache.push((key, c.px.clone())));
    }
}

/// Área's text switch, the settings' switch at half size around (0, 0): grey when
/// off, the accent yellow when on; `h` is how hovered it is.
fn mini_switch(c: &mut Canvas, v: f32, h: f32) {
    c.paint(mix(mix(THUMB, TEXT2, 0.3 * h), YELLOW, v), rrect(-11.0, -6.0, 11.0, 6.0, 6.0));
    c.paint(mix(mix(TEXT2, TEXT, h), BLACK, v), circle(-5.0 + 10.0 * v, 0.0, 4.5));
}

/// What recognizing text will do now, for the switch's tooltip: off, or where the text goes.
pub fn ocr_tip(on: bool, record: bool, clip: bool) -> String {
    match (on, record, clip) {
        (false, ..) => tr!("Text recognition is off", "Reconocimiento de texto apagado", "文字の読み取りはオフです"),
        (true, true, _) => tr!("The text will be saved to a .txt with timestamps", "El texto se guardará en un .txt con marcas de tiempo", "文字をタイムスタンプ付きで .txt に保存します"),
        (true, false, true) => tr!("The text will be copied to the clipboard", "El texto se copiará al portapapeles", "文字をクリップボードにコピーします"),
        (true, false, false) => tr!("The text will be saved to a .txt", "El texto se guardará en un .txt", "文字を .txt に保存します"),
    }
}

/// A tooltip: one line in a dark capsule, the badge's look a little larger.
pub fn tooltip(text: &str, font: Option<&FontVec>, scale: f32) -> Canvas {
    let w = (font.map_or(120.0, |f| Canvas::width(f, text, 13.0)) + 24.0).ceil();
    let mut c = Canvas::new(w as usize, 28, scale);
    c.paint(fade(BLACK, 0.92), rrect(0.0, 0.0, w, 28.0, 14.0));
    c.paint(HAIRLINE, stroke(rrect(0.5, 0.5, w - 0.5, 27.5, 13.5), 1.0));
    if let Some(f) = font {
        c.text(f, text, 13.0, w / 2.0, 18.5, 0.5, TEXT);
    }
    c
}

/// Where the `t`-sized tooltip of Área's switch goes: centred over it, 8 px above the panel, on screen.
pub fn tooltip_pos(sw: i32, sh: i32, scale: f32, (tw, th): (i32, i32)) -> (i32, i32) {
    let (px, py) = place(sw, sh, scale).0;
    let x = px + ((M + cell_mid(0.0)) * scale).round() as i32 - tw / 2;
    let y = py + ((M - 8.0) * scale).round() as i32 - th;
    (x.clamp(0, (sw - tw).max(0)), y.max(0))
}

/// A round close button: `h` is how hovered it is.
fn close_button(c: &mut Canvas, g: Geo, h: f32) {
    let Geo::Disc(x, y, r) = g else { unreachable!() };
    c.paint(mix(WELL, WELL_HI, h), circle(x, y, r));
    c.paint(HAIRLINE, stroke(circle(x, y, r - 0.5), 1.0));
    let col = mix(TEXT2, TEXT, h);
    c.paint(col, line(x - 4.5, y - 4.5, x + 4.5, y + 4.5, 1.1));
    c.paint(col, line(x - 4.5, y + 4.5, x + 4.5, y - 4.5, 1.1));
}

/// The 2 px yellow ring, 3 px clear of `g`.
fn focus_ring(c: &mut Canvas, g: Geo) {
    c.paint(YELLOW, stroke(g.grow(3.0).shape(), 2.0));
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Selection,
    Screen,
    Window,
}

pub const MODES: [Mode; 3] = [Mode::Selection, Mode::Screen, Mode::Window];

fn mode_name(m: Mode) -> String {
    match m {
        Mode::Selection => tr!("Area", "Área", "範囲"),
        Mode::Screen => tr!("Screen", "Pantalla", "画面"),
        Mode::Window => tr!("Window", "Ventana", "ウィンドウ"),
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Hit {
    Close,
    Mode(Mode),
    /// Recognize text: the small switch under Área, there only in that mode.
    Ocr,
    Shot,
    Cast,
    Shutter,
    Settings,
}

/// Keyboard focus order: the strip of modes and Área's switch, then the bottom row left to right.
pub const PANEL_ORDER: [Hit; 9] = [Hit::Mode(Mode::Selection), Hit::Mode(Mode::Screen), Hit::Mode(Mode::Window), Hit::Ocr, Hit::Shot, Hit::Cast, Hit::Shutter, Hit::Settings, Hit::Close];

// Panel geometry, in logical px. The body sits in a transparent margin for its shadow.
const M: f32 = 24.0;
const BODY: (f32, f32) = (320.0, 139.0); // PAD + the strip + 3 + the shutter's hit (68) + PAD
const BODY_R: f32 = 16.0;
/// The body's inner padding around the controls (their hit areas), the same on all four sides.
const PAD: f32 = 12.0;
const PW: usize = 368;
const PH: usize = 187;
const SBODY: (f32, f32) = (BODY.0, SEG_LANG.1 + 20.0); // as wide as the panel: the two cards stack flush
const SBODY_R: f32 = 14.0;
pub const SW: usize = 368; // the settings popover, logical px
pub const SH: usize = SBODY.1 as usize + 2 * M as usize;
/// Each mode word's cell: a third of the strip, 44 tall: the word, and under it
/// the dot, or Área's text switch. All three alike, so none looks bigger.
const CELL: (f32, f32) = ((BODY.0 - 2.0 * PAD) / 3.0, 44.0);
/// The words' baseline and the dot's (or the switch's) centre: centred in the cell together.
const WORD: f32 = PAD + 17.0;
const DOT: f32 = PAD + 30.0;
const ROW: f32 = 93.0; // the bottom row's centre: photo|video, shutter, gear
const SHUTTER: (f32, f32) = (BODY.0 / 2.0, ROW);
/// photo|video and the gear sit under the outer mode words.
const SEG: f32 = SHUTTER.0 - CELL.0;
const GEAR: f32 = SHUTTER.0 + CELL.0;

fn cell_x(i: usize) -> f32 {
    PAD + CELL.0 * i as f32
}

/// The middle of mode cell `i` (fractional while the dot slides).
fn cell_mid(i: f32) -> f32 {
    PAD + CELL.0 * (i + 0.5)
}

/// What a control looks like (and where its focus ring goes).
fn shape_of(h: Hit) -> Geo {
    match h {
        Hit::Mode(m) => {
            let x = cell_x(MODES.iter().position(|&n| n == m).unwrap());
            Geo::Rect(x, PAD, x + CELL.0, PAD + CELL.1, 11.0)
        }
        Hit::Ocr => Geo::Rect(cell_mid(0.0) - 11.0, DOT - 6.0, cell_mid(0.0) + 11.0, DOT + 6.0, 6.0), // the settings' switch at half size
        Hit::Shot => Geo::Rect(SEG - 43.0, ROW - 17.0, SEG, ROW + 17.0, 7.0),
        Hit::Cast => Geo::Rect(SEG, ROW - 17.0, SEG + 43.0, ROW + 17.0, 7.0),
        Hit::Shutter => Geo::Disc(SHUTTER.0, SHUTTER.1, 31.0),
        Hit::Settings => Geo::Disc(GEAR, ROW, 20.0),
        Hit::Close => Geo::Disc(BODY.0 - 1.0, 1.0, 14.0),
    }
}

/// Where a control answers clicks: its look, a little bigger for the small ones.
fn hit_of(h: Hit) -> Geo {
    match h {
        Hit::Ocr => Geo::Rect(cell_mid(0.0) - 24.0, DOT - 7.0, cell_mid(0.0) + 24.0, PAD + CELL.1, 6.0), // the cell below its middle
        Hit::Shot => Geo::Rect(SEG - 48.0, ROW - 22.0, SEG, ROW + 22.0, 11.0),
        Hit::Cast => Geo::Rect(SEG, ROW - 22.0, SEG + 48.0, ROW + 22.0, 11.0),
        Hit::Shutter => Geo::Disc(SHUTTER.0, SHUTTER.1, 34.0),
        Hit::Settings => Geo::Disc(GEAR, ROW, 25.0),
        Hit::Close => Geo::Disc(BODY.0 - 1.0, 1.0, 18.0),
        _ => shape_of(h),
    }
}

/// Canvas size in device px, and the canvas's top-left for the panel and the
/// settings popover on a `sw`×`sh` screen: the panel body 48 px above the
/// bottom, the popover body 18 px above it, both centred. The popover's
/// transparent margin then overhangs the panel's close button; main.rs passes
/// what lands there on to the panel.
pub fn place(sw: i32, sh: i32, scale: f32) -> ((i32, i32), (i32, i32)) {
    let s = |v: f32| v * scale;
    let px = (sw as f32 - s(BODY.0)) / 2.0 - s(M);
    let body_top = sh as f32 - s(48.0) - s(BODY.1);
    let mx = (sw as f32 - s(SBODY.0)) / 2.0 - s(M);
    let my = body_top - s(18.0) - s(SBODY.1) - s(M);
    ((px.round() as i32, (body_top - s(M)).round() as i32), (mx.round() as i32, my.round() as i32))
}

pub struct PanelState<'a> {
    pub mode: Mode,
    pub record: bool,
    pub hover: Option<Hit>,
    pub settings_open: bool,
    pub font: Option<&'a FontVec>,
    /// The medium weight (or `font` again), for the mode words.
    pub bold: Option<&'a FontVec>,
    /// Keyboard focus (None while the ring is hidden).
    pub focus: Option<Hit>,
    /// Recognize text in an area (Área's switch).
    pub ocr: bool,
    pub scale: f32,
    tw: PanelTw,
}

struct PanelTw {
    mode: Tween, // fractional index of the chosen mode
    record: Tween,
    ocr: Tween,
    hover: Hov<Hit>,
}

impl<'a> PanelState<'a> {
    pub fn new(mode: Mode, record: bool, (font, bold): (Option<&'a FontVec>, Option<&'a FontVec>), scale: f32) -> Self {
        let ix = MODES.iter().position(|&m| m == mode).unwrap() as f32;
        let tw = PanelTw { mode: Tween::io(ix), record: Tween::io(record as u8 as f32), ocr: Tween::io(0.0), hover: Hov::new() };
        PanelState { mode, record, hover: None, settings_open: false, font, bold, focus: None, ocr: false, scale, tw }
    }

    /// Aim the tweens at the current state; call before every render.
    pub fn sync(&mut self) {
        let ix = MODES.iter().position(|&m| m == self.mode).unwrap() as f32;
        self.tw.mode.go(ix, 200.0);
        self.tw.record.go(self.record as u8 as f32, 180.0);
        self.tw.ocr.go(self.ocr as u8 as f32, 160.0);
        self.tw.hover.set(self.hover);
    }

    pub fn busy(&self) -> bool {
        let t = &self.tw;
        t.mode.busy() || t.record.busy() || t.ocr.busy() || t.hover.t.busy()
    }

    /// Jump every tween to the state (previews).
    #[cfg(test)]
    pub fn settle(&mut self) {
        self.sync();
        let t = &mut self.tw;
        for w in [&mut t.mode, &mut t.record, &mut t.ocr] {
            w.settle();
        }
        t.hover.settle(self.hover);
    }
}

/// The control under device px (x, y); `switch`: Área's switch is there (Área mode).
pub fn panel_hit(scale: f32, x: i16, y: i16, switch: bool) -> Option<Hit> {
    let (x, y) = (x as f32 / scale - M, y as f32 / scale - M);
    PANEL_ORDER.into_iter().rev().filter(|&h| switch || h != Hit::Ocr).find(|&h| hit_of(h).has(x, y))
}

/// Whether device px (x, y) is on the panel's card or a control (the close button overhangs); the rest of the canvas is transparent.
pub fn panel_body_has(scale: f32, x: i16, y: i16) -> bool {
    let (x, y) = (x as f32 / scale - M, y as f32 / scale - M);
    sd_rrect(x, y, (0.0, 0.0, BODY.0, BODY.1), BODY_R) <= 0.0 || PANEL_ORDER.into_iter().any(|h| hit_of(h).has(x, y))
}

/// The launcher, laid out like a phone camera: the modes as a strip of words,
/// the shutter in the middle, photo|video on its left and the settings on its right.
pub fn panel(s: &PanelState) -> Canvas {
    let mut c = Canvas::new(PW, PH, s.scale);
    let t = &s.tw;
    c.origin = (M, M);
    card(&mut c, BODY.0, BODY.1, BODY_R);
    let (idx, rec) = (t.mode.get(), t.record.get());
    let hv = |h| t.hover.amt(h);

    // the modes: words in big cells; the chosen one yellow, with a dot that slides
    for (i, &m) in MODES.iter().enumerate() {
        let Geo::Rect(x0, y0, x1, y1, r) = shape_of(Hit::Mode(m)) else { unreachable!() };
        let (h, sel) = (hv(Hit::Mode(m)), (1.0 - (idx - i as f32).abs()).clamp(0.0, 1.0));
        c.paint(fade(WHITE, 0.06 * h), rrect(x0, y0, x1, y1, r));
        if let Some(f) = s.bold.or(s.font) {
            let (word, px, track) = caps(&mode_name(m), 13.0);
            c.spaced(f, &word, px, (cell_mid(i as f32), WORD), 0.5, (track, false), mix(mix(TEXT2, TEXT, h), YELLOW, sel));
        }
    }
    // Arriving at Área the dot grows into the text switch, leaving it the switch shrinks back into the dot.
    let area = (1.0 - idx).clamp(0.0, 1.0);
    c.alpha = 1.0 - area;
    c.paint(YELLOW, circle(cell_mid(idx), DOT, 2.5));
    c.alpha = area;
    c.scaled((cell_mid(0.0), DOT), 0.4 + 0.6 * area, |c| mini_switch(c, t.ocr.get(), hv(Hit::Ocr)));
    c.alpha = 1.0;

    // photo | video
    c.paint(WELL, rrect(SEG - 46.0, ROW - 20.0, SEG + 46.0, ROW + 20.0, 10.0));
    c.paint(THUMB, rrect(SEG - 43.0 + 43.0 * rec, ROW - 17.0, SEG + 43.0 * rec, ROW + 17.0, 7.0)); // concentric: the well's 10 less its 3 px inset
    let on = |h: Hit, w: f32| mix(mix(TEXT2, TEXT, hv(h)), TEXT, w);
    icon_camera(&mut c, SEG - 21.5, ROW, on(Hit::Shot, 1.0 - rec), mix(WELL, THUMB, 1.0 - rec));
    c.scaled((SEG + 21.5, ROW), 0.9, |c| icon_video(c, 0.0, 0.0, on(Hit::Cast, rec)));

    // the shutter: white for a photo, red for video; the disc draws in a little under the pointer
    let (sx, sy) = SHUTTER;
    c.paint(TEXT, stroke(circle(sx, sy, 29.0), 3.5));
    c.paint(mix(TEXT, RED, rec), circle(sx, sy, 24.0 - 2.0 * hv(Hit::Shutter)));

    // settings
    let (open, gh) = (s.settings_open, hv(Hit::Settings));
    let Geo::Disc(gx, gy, gr) = shape_of(Hit::Settings) else { unreachable!() };
    let gbg = if open { THUMB } else { mix(WELL, WELL_HI, gh) };
    c.paint(gbg, circle(gx, gy, gr));
    c.scaled((gx, gy), 0.85, |c| icon_gear(c, 0.0, 0.0, if open { TEXT } else { mix(TEXT2, TEXT, gh) }, gbg));

    close_button(&mut c, shape_of(Hit::Close), hv(Hit::Close));
    if let Some(g) = s.focus {
        focus_ring(&mut c, shape_of(g));
    }
    c
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SetHit {
    Close,
    Output(usize),
    Mic,
    VideoFormat(usize),
    Gpu,
    ImageFormat(usize),
    Clip,
    Pointer,
    Shortcut,
    Lang(usize),
}

/// Keyboard focus order.
pub const SETTINGS_ORDER: [SetHit; 16] = [
    SetHit::Output(0),
    SetHit::Output(1),
    SetHit::Output(2),
    SetHit::Mic,
    SetHit::VideoFormat(0),
    SetHit::VideoFormat(1),
    SetHit::Gpu,
    SetHit::ImageFormat(0),
    SetHit::ImageFormat(1),
    SetHit::Clip,
    SetHit::Pointer,
    SetHit::Shortcut,
    SetHit::Lang(0),
    SetHit::Lang(1),
    SetHit::Lang(2),
    SetHit::Close,
];

// Settings content runs between SX.0 and SX.1; the pickers and the shortcut field fill a right-hand column from PICK.
const SX: (f32, f32) = (20.0, SBODY.0 - 20.0);
const PICK: f32 = SX.1 - 124.0;
// Settings rows (centres) in the body.
const SEG_OUT: (f32, f32) = (84.0, 116.0);
const ROW_MIC: f32 = 140.0;
const ROW_VIDEO: f32 = 190.0;
const ROW_GPU: f32 = 230.0;
const ROW_SHOT: f32 = 278.0;
const ROW_CLIP: f32 = 318.0;
const ROW_POINTER: f32 = 358.0;
const ROW_KEY: f32 = 406.0;
const SEG_LANG: (f32, f32) = (466.0, 498.0);

/// Cell `i` of `n` across x0..x1 and y0..y1; `inset` 3 gives the thumb, its corners concentric with the well's.
fn cell(x0: f32, x1: f32, (y0, y1): (f32, f32), n: usize, i: usize, inset: f32) -> Geo {
    let w = (x1 - x0) / n as f32;
    let (l, t) = (x0 + w * i as f32 + inset, y0 + inset);
    Geo::Rect(l, t, l + w - 2.0 * inset, y1 - inset, (y1 - y0) / 4.0 - inset)
}

fn pick_y(cy: f32) -> (f32, f32) {
    (cy - 16.0, cy + 16.0)
}

fn row_rect(cy: f32) -> Geo {
    Geo::Rect(8.0, cy - 18.0, SBODY.0 - 8.0, cy + 18.0, 6.0)
}

fn switch_geo(cy: f32) -> Geo {
    Geo::Rect(SX.1 - 44.0, cy - 12.0, SX.1, cy + 12.0, 12.0)
}

/// What a settings control looks like (and where its focus ring goes).
fn set_shape(h: SetHit) -> Geo {
    match h {
        SetHit::Close => Geo::Disc(SX.1 - 10.0, 30.0, 14.0),
        SetHit::Output(i) => cell(SX.0, SX.1, SEG_OUT, 3, i, 3.0),
        SetHit::Lang(i) => cell(SX.0, SX.1, SEG_LANG, 3, i, 3.0),
        SetHit::VideoFormat(i) => cell(PICK, SX.1, pick_y(ROW_VIDEO), 2, i, 3.0),
        SetHit::ImageFormat(i) => cell(PICK, SX.1, pick_y(ROW_SHOT), 2, i, 3.0),
        SetHit::Mic => switch_geo(ROW_MIC),
        SetHit::Gpu => switch_geo(ROW_GPU),
        SetHit::Pointer => switch_geo(ROW_POINTER),
        SetHit::Clip => switch_geo(ROW_CLIP),
        SetHit::Shortcut => Geo::Rect(PICK, ROW_KEY - 16.0, SX.1, ROW_KEY + 16.0, 8.0),
    }
}

/// Where a settings control answers clicks: whole cells and whole rows.
fn set_hit(h: SetHit) -> Geo {
    match h {
        SetHit::Close => Geo::Disc(SX.1 - 10.0, 30.0, 18.0),
        SetHit::Output(i) => cell(SX.0, SX.1, SEG_OUT, 3, i, 0.0),
        SetHit::Lang(i) => cell(SX.0, SX.1, SEG_LANG, 3, i, 0.0),
        SetHit::VideoFormat(i) => cell(PICK, SX.1, pick_y(ROW_VIDEO), 2, i, 0.0),
        SetHit::ImageFormat(i) => cell(PICK, SX.1, pick_y(ROW_SHOT), 2, i, 0.0),
        SetHit::Mic => row_rect(ROW_MIC),
        SetHit::Gpu => row_rect(ROW_GPU),
        SetHit::Pointer => row_rect(ROW_POINTER),
        SetHit::Clip => row_rect(ROW_CLIP),
        SetHit::Shortcut => set_shape(h),
    }
}

pub fn settings_hit(scale: f32, x: i16, y: i16) -> Option<SetHit> {
    let (x, y) = (x as f32 / scale - M, y as f32 / scale - M);
    SETTINGS_ORDER.into_iter().find(|&h| set_hit(h).has(x, y))
}

/// Like `panel_body_has`, for the settings popover.
pub fn settings_body_has(scale: f32, x: i16, y: i16) -> bool {
    let (x, y) = (x as f32 / scale - M, y as f32 / scale - M);
    sd_rrect(x, y, (0.0, 0.0, SBODY.0, SBODY.1), SBODY_R) <= 0.0 || SETTINGS_ORDER.into_iter().any(|h| set_hit(h).has(x, y))
}

pub struct SettingsState<'a> {
    pub output: usize, // index into audio::OUTPUTS
    pub mic: bool,
    pub mp4: bool,
    pub gpu: bool,
    pub gpu_found: bool,
    pub jpg: bool,
    pub pointer: bool,
    /// Copy photos to the clipboard; off limits while the panel is on video (`record`).
    pub clip: bool,
    pub record: bool,
    /// Text will be recognized (Área mode, its switch on): then the text is what gets copied.
    pub text: bool,
    pub shortcut: String,
    pub capturing: bool, // waiting for the new shortcut
    pub hover: Option<SetHit>,
    pub focus: Option<SetHit>,
    pub font: Option<&'a FontVec>,
    /// The medium weight (or `font` again), for the title and section labels.
    pub bold: Option<&'a FontVec>,
    /// A font with 日本語, for the language picker when `font` has none.
    pub cjk: Option<&'a FontVec>,
    pub scale: f32,
    tw: SetTw,
}

struct SetTw {
    appear: Tween,
    out: Tween,
    mic: Tween,
    video: Tween,
    gpu: Tween,
    image: Tween,
    pointer: Tween,
    lang: Tween,
    clip: Tween,
    hover: Hov<SetHit>,
}

impl SetTw {
    fn all(&mut self) -> [&mut Tween; 9] {
        [&mut self.appear, &mut self.out, &mut self.mic, &mut self.video, &mut self.gpu, &mut self.image, &mut self.pointer, &mut self.lang, &mut self.clip]
    }

    fn busy(&self) -> bool {
        [&self.appear, &self.out, &self.mic, &self.video, &self.gpu, &self.image, &self.pointer, &self.lang, &self.clip].iter().any(|t| t.busy()) || self.hover.t.busy()
    }
}

fn lang_ix() -> f32 {
    crate::i18n::LANGS.iter().position(|&l| l == crate::i18n::lang()).unwrap() as f32
}

impl<'a> SettingsState<'a> {
    /// All off, with the tweens settled; set the fields, then `settle`.
    pub fn new((font, bold): (Option<&'a FontVec>, Option<&'a FontVec>), cjk: Option<&'a FontVec>, scale: f32) -> Self {
        let tw = SetTw {
            appear: Tween::new(1.0),
            out: Tween::io(0.0),
            mic: Tween::io(0.0),
            video: Tween::io(0.0),
            gpu: Tween::io(0.0),
            image: Tween::io(0.0),
            pointer: Tween::io(0.0),
            lang: Tween::io(lang_ix()),
            clip: Tween::io(0.0),
            hover: Hov::new(),
        };
        SettingsState { output: 0, mic: false, mp4: false, gpu: false, gpu_found: false, jpg: false, pointer: false, clip: false, record: false, text: false, shortcut: String::new(), capturing: false, hover: None, focus: None, font, bold, cjk, scale, tw }
    }

    pub fn sync(&mut self) {
        let f = |b: bool| b as u8 as f32;
        let t = &mut self.tw;
        t.out.go(self.output as f32, 180.0);
        t.video.go(f(self.mp4), 180.0);
        t.image.go(f(self.jpg), 180.0);
        t.lang.go(lang_ix(), 180.0);
        t.mic.go(f(self.mic), 160.0);
        t.gpu.go(f(self.gpu && self.gpu_found), 160.0);
        t.pointer.go(f(self.pointer), 160.0);
        t.clip.go(f(self.clip), 160.0);
        t.hover.set(self.hover);
    }

    pub fn busy(&self) -> bool {
        self.tw.busy()
    }

    pub fn reveal(&mut self) {
        self.tw.appear = appear(160.0);
    }

    pub fn settle(&mut self) {
        self.sync();
        self.tw.all().into_iter().for_each(Tween::settle);
        self.tw.hover.settle(self.hover);
    }
}

/// On: a white track with a black knob; off: a grey track with a white one; disabled: dim, the knob where it was.
fn switch(c: &mut Canvas, cy: f32, v: f32, enabled: bool) {
    let Geo::Rect(x0, y0, x1, y1, r) = switch_geo(cy) else { unreachable!() };
    c.paint(if enabled { mix(THUMB, TEXT, v) } else { WELL }, rrect(x0, y0, x1, y1, r));
    c.paint(if enabled { mix(TEXT, BLACK, v) } else { mix(DISABLED, THUMB, v) }, circle(x0 + 12.0 + 20.0 * v, cy, 9.0));
}

/// Segmented control in the rect x0..x1 × `ys`: `labels` in a capsule, the thumb at the fractional index `on`.
fn segmented(c: &mut Canvas, (x0, x1): (f32, f32), ys: (f32, f32), labels: &[(&str, Option<&FontVec>)], on: f32, hover: &dyn Fn(usize) -> f32) {
    let n = labels.len();
    let Geo::Rect(l, t, r, b, rad) = cell(x0, x1, ys, 1, 0, 0.0) else { unreachable!() };
    c.paint(WELL, rrect(l, t, r, b, rad));
    for i in 0..n {
        let Geo::Rect(l, t, r, b, rad) = cell(x0, x1, ys, n, i, 3.0) else { unreachable!() };
        let sel = (1.0 - (on - i as f32).abs()).clamp(0.0, 1.0);
        c.paint(fade(WHITE, 0.035 * hover(i) * (1.0 - sel)), rrect(l, t, r, b, rad)); // fainter than the thumb
    }
    let Geo::Rect(l, t, r, b, rad) = cell(x0, x1, ys, n, 0, 3.0) else { unreachable!() };
    let dx = (x1 - x0) / n as f32 * on;
    c.paint(THUMB, rrect(l + dx, t, r + dx, b, rad));
    for (i, (label, font)) in labels.iter().enumerate() {
        let sel = (1.0 - (on - i as f32).abs()).clamp(0.0, 1.0);
        if let Some(f) = font {
            let cx = x0 + (x1 - x0) / n as f32 * (i as f32 + 0.5);
            c.text(f, label, 14.0, cx, (ys.0 + ys.1) / 2.0 + 5.0, 0.5, mix(mix(TEXT2, TEXT, 0.5 * hover(i)), TEXT, sel));
        }
    }
}

pub fn settings(s: &SettingsState) -> Canvas {
    let mut c = Canvas::new(SW, SH, s.scale);
    let t = &s.tw;
    let a = t.appear.get();
    (c.alpha, c.origin) = (a, (M, M + (1.0 - a) * 10.0));
    card(&mut c, SBODY.0, SBODY.1, SBODY_R);
    let hv = |h| t.hover.amt(h);
    let f = s.font;
    let lbl = |c: &mut Canvas, text: &str, px: f32, x: f32, base: f32, col: Rgba| {
        if let Some(f) = f {
            c.text(f, text, px, x, base, 0.0, col);
        }
    };
    // a section's name, set like the mode words
    let section = |c: &mut Canvas, text: &str, base: f32| {
        if let Some(b) = s.bold.or(f) {
            let (text, px, track) = caps(text, 11.5);
            c.spaced(b, &text, px, (20.0, base), 0.0, (track, false), TEXT2);
        }
    };

    if let Some(b) = s.bold.or(f) {
        c.text(b, &tr!("Settings", "Ajustes", "設定"), 17.0, 20.0, 36.0, 0.0, TEXT);
    }
    close_button(&mut c, set_shape(SetHit::Close), hv(SetHit::Close));

    // sound
    section(&mut c, &tr!("Sound", "Sonido", "サウンド"), 74.0);
    let sound = [tr!("None", "Sin sonido", "なし"), tr!("System", "Sistema", "システム"), tr!("App", "Aplicación", "アプリ")];
    let labels: Vec<(&str, Option<&FontVec>)> = sound.iter().map(|l| (l.as_str(), f)).collect();
    segmented(&mut c, SX, SEG_OUT, &labels, t.out.get(), &|i| hv(SetHit::Output(i)));

    // rows: whole switch rows react to the pointer
    for (h, cy) in [(SetHit::Mic, ROW_MIC), (SetHit::Gpu, ROW_GPU), (SetHit::Clip, ROW_CLIP), (SetHit::Pointer, ROW_POINTER)] {
        if (h != SetHit::Gpu || s.gpu_found) && (h != SetHit::Clip || !s.record) {
            let Geo::Rect(x0, y0, x1, y1, r) = row_rect(cy) else { unreachable!() };
            c.paint(fade(WHITE, 0.05 * hv(h)), rrect(x0, y0, x1, y1, r));
        }
    }
    lbl(&mut c, &tr!("Microphone", "Micrófono", "マイク"), 15.0, 20.0, ROW_MIC + 5.0, TEXT);
    switch(&mut c, ROW_MIC, t.mic.get(), true);
    for y in [166.0, 254.0, ROW_POINTER + 24.0, ROW_KEY + 26.0] {
        c.paint(DIVIDER, line(SX.0, y, SX.1, y, 0.5));
    }

    lbl(&mut c, &tr!("Video format", "Formato de video", "動画の形式"), 15.0, 20.0, ROW_VIDEO + 5.0, TEXT);
    segmented(&mut c, (PICK, SX.1), pick_y(ROW_VIDEO), &[("MKV", f), ("MP4", f)], t.video.get(), &|i| hv(SetHit::VideoFormat(i)));

    let use_gpu = tr!("Use GPU", "Usar GPU", "GPU を使う");
    lbl(&mut c, &use_gpu, 15.0, 20.0, ROW_GPU + 5.0, if s.gpu_found { TEXT } else { TEXT2 });
    if let Some(f) = f {
        let note = if s.gpu_found { "NVENC".to_owned() } else { tr!("not available", "no disponible", "利用不可") };
        c.text(f, &note, 12.0, 20.0 + Canvas::width(f, &use_gpu, 15.0) + 8.0, ROW_GPU + 5.0, 0.0, TEXT2);
    }
    switch(&mut c, ROW_GPU, t.gpu.get(), s.gpu_found);

    lbl(&mut c, &tr!("Image format", "Formato de imagen", "画像の形式"), 15.0, 20.0, ROW_SHOT + 5.0, TEXT);
    segmented(&mut c, (PICK, SX.1), pick_y(ROW_SHOT), &[("PNG", f), ("JPG", f)], t.image.get(), &|i| hv(SetHit::ImageFormat(i)));

    // the clipboard: photos only; the note says what goes there, the image or the recognized text
    let copy = tr!("Copy to clipboard", "Copiar al portapapeles", "クリップボードにコピー");
    lbl(&mut c, &copy, 15.0, 20.0, ROW_CLIP + 5.0, if s.record { TEXT2 } else { TEXT });
    if let Some(f) = f {
        let what = match (s.record, s.text) {
            (true, _) => tr!("photos only", "solo fotos", "写真のみ"),
            (false, false) => tr!("the image", "la imagen", "画像"),
            (false, true) => tr!("the text", "el texto", "文字"),
        };
        c.text(f, &what, 12.0, 20.0 + Canvas::width(f, &copy, 15.0) + 8.0, ROW_CLIP + 5.0, 0.0, TEXT2);
    }
    switch(&mut c, ROW_CLIP, t.clip.get(), !s.record);

    lbl(&mut c, &tr!("Show pointer", "Mostrar cursor", "ポインターを表示"), 15.0, 20.0, ROW_POINTER + 5.0, TEXT);
    switch(&mut c, ROW_POINTER, t.pointer.get(), true);

    // shortcut: shows the current one; click, then press the new keys
    lbl(&mut c, &tr!("Shortcut", "Atajo", "ショートカット"), 15.0, 20.0, ROW_KEY - 4.0, TEXT);
    let caption = if s.capturing { tr!("Esc to cancel", "Esc para cancelar", "Esc でキャンセル") } else { tr!("Opens screenrec", "Abre screenrec", "screenrec を開く") };
    lbl(&mut c, &caption, 12.0, 20.0, ROW_KEY + 12.0, TEXT2);
    let Geo::Rect(x0, y0, x1, y1, r) = set_shape(SetHit::Shortcut) else { unreachable!() };
    c.paint(if s.capturing { TEXT } else { mix(WELL, WELL_HI, hv(SetHit::Shortcut)) }, rrect(x0, y0, x1, y1, r));
    if let Some(f) = f {
        let (text, col) = if s.capturing { (tr!("Press keys…", "Presiona…", "キーを入力…"), BLACK) } else { (s.shortcut.clone(), TEXT) };
        c.text(f, &Canvas::fit(f, &text, 14.0, 108.0), 14.0, (x0 + x1) / 2.0, ROW_KEY + 5.0, 0.5, col);
    }

    // language: each in its own name, so a wrong pick can be undone
    section(&mut c, &tr!("Language", "Idioma", "言語"), SEG_LANG.0 - 10.0);
    let ja = if s.cjk.is_some() || crate::i18n::lang() == crate::i18n::Lang::Ja { "日本語".to_owned() } else { tr!("Japanese", "Japonés", "日本語") };
    let langs = [("English", f), ("Español", f), (ja.as_str(), s.cjk.or(f))];
    segmented(&mut c, SX, SEG_LANG, &langs, t.lang.get(), &|i| hv(SetHit::Lang(i)));

    if let Some(h) = s.focus {
        focus_ring(&mut c, set_shape(h));
    }
    c
}

/// The pill's word before the time: REC, or Paused.
fn pill_label(paused: bool) -> (String, f32, f32) {
    if paused { caps(&tr!("Paused", "Pausa", "一時停止"), 11.0) } else { ("REC".into(), 11.0, 1.1) }
}

/// Where the pill's parts go (logical px): the time, the divider, the whole width.
/// Room for either word, so pausing never moves the buttons.
fn pill_geo(font: Option<&FontVec>, bold: Option<&FontVec>) -> (f32, f32, f32) {
    let word = |paused| bold.or(font).map_or(0.0, |f| {
        let (s, px, track) = pill_label(paused);
        spaced_width(f, &s, px, track, false)
    });
    let time = (28.0 + word(false)).max(16.0 + word(true)) + 10.0;
    let div = time + 60.0; // H:MM:SS fits
    (time, div, (div + 62.0).ceil())
}

/// Recording pill button under device x (1 pause/resume, 2 stop) on a pill whose divider is at `div`.
pub fn pill_slot(scale: f32, x: i16, div: f32) -> usize {
    match x as f32 / scale - div {
        x if x < 0.0 => 0,
        x if x < 30.0 => 1,
        _ => 2,
    }
}

/// M:SS, or H:MM:SS from an hour on.
fn clock(secs: u64) -> String {
    match secs {
        s if s < 3600 => format!("{}:{:02}", s / 60, s % 60),
        s => format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60),
    }
}

/// The recording pill, a camera's REC readout: "● REC 0:42 | ❚❚ ■". Translucent
/// on purpose: total alpha stays <= 0.8 (0.9 on the small dot), so capture.rs
/// can solve the compositor's blend for the pixels underneath with an error of
/// a couple of levels.
pub fn pill(paused: bool, secs: u64, (font, bold): (Option<&FontVec>, Option<&FontVec>), scale: f32) -> Canvas {
    let (time, div, w) = pill_geo(font, bold);
    let mut c = Canvas::new(w as usize, 40, scale);
    c.paint((0.03, 0.03, 0.035, 0.6), rrect(0.0, 0.0, w, 40.0, 20.0));
    let ink = (1.0, 1.0, 1.0, 0.5); // 0.5 over the 0.6 backdrop -> 0.8 total
    if !paused {
        c.paint((1.0, 0.27, 0.22, 0.75), circle(18.0, 20.0, 4.5));
    }
    if let Some(b) = bold.or(font) {
        let (word, px, track) = pill_label(paused);
        c.spaced(b, &word, px, (if paused { 16.0 } else { 28.0 }, 24.0), 0.0, (track, false), ink); // red would be too faint at this alpha
    }
    if let Some(f) = font {
        c.spaced(f, &clock(secs), 15.0, (time, 25.5), 0.0, (0.0, true), ink);
    }
    c.paint((1.0, 1.0, 1.0, 0.16), line(div, 12.0, div, 28.0, 0.5));
    let px = div + 17.0;
    if paused {
        c.poly(ink, &[(px - 4.5, 13.0), (px + 6.5, 20.0), (px - 4.5, 27.0)]);
    } else {
        c.paint(ink, line(px - 3.5, 14.0, px - 3.5, 26.0, 1.3));
        c.paint(ink, line(px + 3.5, 14.0, px + 3.5, 26.0, 1.3));
    }
    c.paint(ink, rrect(div + 38.0, 14.0, div + 50.0, 26.0, 2.5));
    c
}

/// The whole launcher's opacity, which the compositor applies through each window's
/// _NET_WM_WINDOW_OPACITY: fading in or out redraws nothing here, a frame costs four
/// small requests. Without a compositing manager nothing can fade, so it doesn't.
pub struct Fade {
    atom: u32,
    tw: Tween,
    shown: Option<u32>, // the value last set
    on: bool,           // a compositor applies it
    full: f32,          // ms for a fade all the way
}

/// A fade all the way, in or out.
const FADE_MS: f32 = 500.0;
/// Out before a recording, which can't start while any of the launcher shows: 17
/// refreshes at 144 Hz, the first taking 16 % of the way and each next one less,
/// so it still reads as a fade, not a cut. Every ms more delays the recording.
const REC_FADE_MS: f32 = 120.0;

impl Fade {
    /// Fully transparent until `go`.
    pub fn new(cap: &Capture) -> Res<Self> {
        let conn = &cap.conn;
        let screen = conn.setup().roots.iter().position(|s| s.root == cap.root).unwrap_or(0);
        let cm = conn.intern_atom(false, format!("_NET_WM_CM_S{screen}").as_bytes())?;
        let atom = conn.intern_atom(false, b"_NET_WM_WINDOW_OPACITY")?.reply()?.atom;
        let on = conn.get_selection_owner(cm.reply()?.atom)?.reply()?.owner != x11rb::NONE;
        Ok(Fade { atom, tw: Tween::new(0.0), shown: None, on, full: FADE_MS })
    }

    /// Head for opacity `to`: half a second for the whole way (see `hurry`), less for
    /// part of it; at once without a compositor or with reduced motion.
    pub fn go(&mut self, to: f32) {
        let ms = if self.on { self.full * (to - self.tw.get()).abs() } else { 0.0 };
        self.tw.go(to, ms);
    }

    /// Fade out quickly from now on: a recording is waiting for the launcher to go.
    pub fn hurry(&mut self) {
        self.full = REC_FADE_MS;
    }

    pub fn busy(&self) -> bool {
        self.tw.busy()
    }

    /// Put the current opacity on `wins`, if it changed since the last call.
    pub fn apply(&mut self, conn: &impl Connection, wins: &[u32]) -> Res<()> {
        let v = (self.tw.get().clamp(0.0, 1.0) as f64 * u32::MAX as f64).round() as u32;
        if self.shown != Some(v) {
            for &w in wins {
                conn.change_property32(PropMode::REPLACE, w, self.atom, AtomEnum::CARDINAL, &[v])?;
            }
            self.shown = Some(v);
        }
        Ok(())
    }
}

/// Override-redirect ARGB window: no decorations, no WM animations, stays on top.
pub struct Win {
    pub id: u32,
    gc: u32,
    pub x: i32,
    pub y: i32,
    pub canvas: Canvas,
}

impl Win {
    pub fn new(cap: &Capture, x: i32, y: i32, canvas: Canvas, events: EventMask) -> Res<Self> {
        let conn = &cap.conn;
        let screen = conn.setup().roots.iter().find(|s| s.root == cap.root).ok_or(tr!("no screen", "sin pantalla", "画面がありません"))?;
        let visual = screen
            .allowed_depths
            .iter()
            .filter(|d| d.depth == 32)
            .flat_map(|d| &d.visuals)
            .find(|v| v.class == VisualClass::TRUE_COLOR)
            .ok_or(tr!("the X server has no ARGB visual (no compositor?)", "el servidor X no tiene visual ARGB (¿sin compositor?)", "X サーバーに ARGB ビジュアルがありません (コンポジターなし?)"))?
            .visual_id;
        let cmap = conn.generate_id()?;
        conn.create_colormap(ColormapAlloc::NONE, cmap, cap.root, visual)?;
        let id = conn.generate_id()?;
        let aux = CreateWindowAux::new().background_pixel(0).border_pixel(0).colormap(cmap).override_redirect(1).event_mask(events);
        let (w, h) = (canvas.w as u16, canvas.h as u16);
        conn.create_window(32, id, cap.root, x as i16, y as i16, w, h, 0, WindowClass::INPUT_OUTPUT, visual, &aux)?;
        conn.change_property8(PropMode::REPLACE, id, AtomEnum::WM_NAME, AtomEnum::STRING, b"screenrec")?;
        let gc = conn.generate_id()?;
        conn.create_gc(gc, id, &CreateGCAux::new())?;
        Ok(Win { id, gc, x, y, canvas })
    }

    pub fn show(&self, conn: &impl Connection) -> Res<()> {
        conn.map_window(self.id)?;
        self.draw(conn)
    }

    pub fn draw(&self, conn: &impl Connection) -> Res<()> {
        let bytes: Vec<u8> = self.canvas.px.iter().flat_map(|p| p.to_le_bytes()).collect();
        let (w, h) = (self.canvas.w as u16, self.canvas.h as u16);
        conn.put_image(ImageFormat::Z_PIXMAP, self.id, self.gc, w, h, 0, 0, 0, 32, &bytes)?;
        Ok(())
    }

    pub fn redraw(&mut self, conn: &impl Connection, canvas: Canvas) -> Res<()> {
        self.canvas = canvas;
        self.draw(conn)
    }

    /// Move and resize to `canvas`, and draw it.
    pub fn reset(&mut self, conn: &impl Connection, x: i32, y: i32, canvas: Canvas) -> Res<()> {
        conn.configure_window(self.id, &ConfigureWindowAux::new().x(x).y(y).width(canvas.w as u32).height(canvas.h as u32))?;
        (self.x, self.y) = (x, y);
        self.redraw(conn, canvas)
    }

    pub fn move_to(&mut self, conn: &impl Connection, x: i32, y: i32) -> Res<()> {
        conn.configure_window(self.id, &ConfigureWindowAux::new().x(x).y(y))?;
        (self.x, self.y) = (x, y);
        Ok(())
    }

    /// What capture.rs needs to recognise (or remove) this window in a frame.
    pub fn sprite(&self) -> Sprite {
        let c = &self.canvas;
        Sprite { x: self.x, y: self.y, w: c.w as i32, h: c.h as i32, argb: c.px.clone() }
    }
}

/// The control after (or, `back`, before) `cur` in `order`, wrapping and passing over `skip`ped ones.
pub fn step<T: Copy + PartialEq>(order: &[T], cur: T, back: bool, skip: impl Fn(T) -> bool) -> T {
    let (n, i) = (order.len(), order.iter().position(|&h| h == cur).unwrap_or(0));
    (1..=n).map(|d| order[(i + if back { n - d } else { d }) % n]).find(|&h| !skip(h)).unwrap_or(cur)
}

/// The size badge: "1280 × 720" in tabular digits, so a drag doesn't make it
/// jitter, after the window's name in Window mode; a 24 px capsule just big enough.
pub fn badge(w: i32, h: i32, name: Option<&str>, font: Option<&FontVec>, scale: f32) -> Canvas {
    let dims = format!("{w} \u{d7} {h}");
    let name = name.zip(font).map(|(n, f)| Canvas::fit(f, n, 12.5, 220.0));
    let dw = font.map_or(60.0, |f| spaced_width(f, &dims, 12.5, 0.0, true));
    let nw = name.as_ref().zip(font).map_or(0.0, |(n, f)| Canvas::width(f, n, 12.5) + 8.0);
    let bw = (nw + dw + 20.0).ceil();
    let mut c = Canvas::new(bw as usize, 24, scale);
    c.paint(fade(BLACK, 0.9), rrect(0.0, 0.0, bw, 24.0, 12.0));
    c.paint(HAIRLINE, stroke(rrect(0.5, 0.5, bw - 0.5, 23.5, 11.5), 1.0));
    if let Some(f) = font {
        if let Some(n) = &name {
            c.text(f, n, 12.5, 10.0, 16.5, 0.0, TEXT);
        }
        c.spaced(f, &dims, 12.5, (10.0 + nw, 16.5), 0.0, (0.0, true), if name.is_some() { TEXT2 } else { TEXT });
    }
    c
}

/// Where the `b`-sized badge goes for selection `sel` on an `sw`×`sh` screen: `gap` px under
/// its bottom-left corner, else (off screen, or behind `avoid`, the panel) inside its top-left one;
/// always on screen.
pub fn badge_pos(sel: Rect, (bw, bh): (i32, i32), (sw, sh): (i32, i32), gap: i32, avoid: Rect) -> (i32, i32) {
    let fit = |(x, y): (i32, i32)| (x.min(sw - bw).max(0), y.min(sh - bh).max(0));
    let (x, y) = fit((sel.0, sel.3 + gap));
    let hidden = x < avoid.2 && avoid.0 < x + bw && y < avoid.3 && avoid.1 < y + bh;
    if sel.3 + gap + bh <= sh && !hidden { (x, y) } else { fit((sel.0 + gap, sel.1 + gap)) }
}

/// The panel's body on a `sw`×`sh` screen, in device px.
pub fn panel_rect(sw: i32, sh: i32, scale: f32) -> Rect {
    let ((x, y), m) = (place(sw, sh, scale).0, (M * scale).round() as i32);
    (x + m, y + m, x + m + (BODY.0 * scale).round() as i32, y + m + (BODY.1 * scale).round() as i32)
}

const EDGE: i32 = 20; // where the pill rests, from the screen edge (logical px)

pub enum PillEvent {
    None,
    TogglePause,
    Stop,
}

struct Drag {
    root: (i32, i32),
    from: (i32, i32),
    press_x: i16,
    moved: bool,
}

/// Start time, from, to.
type Glide = (Instant, (i32, i32), (i32, i32));

/// The recording pill: click to pause/stop, drag it anywhere and on release
/// it glides to the nearest screen edge.
pub struct Pill<'a> {
    pub win: Win,
    fonts: (Option<&'a FontVec>, Option<&'a FontVec>),
    scale: f32,
    div: f32, // where the buttons start, logical px
    started: Instant,
    paused: Option<Instant>,
    paused_for: Duration,
    secs: u64, // shown
    screen: (i32, i32),
    drag: Option<Drag>,
    glide: Option<Glide>,
}

impl<'a> Pill<'a> {
    /// `fonts`: regular and medium, as for the panel.
    pub fn new(cap: &Capture, fonts: (Option<&'a FontVec>, Option<&'a FontVec>), scale: f32) -> Res<Self> {
        let c = pill(false, 0, fonts, scale);
        let (sw, sh) = (cap.sw as i32, cap.sh as i32);
        let edge = (EDGE as f32 * scale).round() as i32;
        let (x, y) = (sw - c.w as i32 - edge, sh - c.h as i32 - edge);
        let mask = EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::BUTTON1_MOTION;
        let win = Win::new(cap, x, y, c, mask)?;
        win.show(&cap.conn)?;
        Ok(Pill { win, fonts, scale, div: pill_geo(fonts.0, fonts.1).1, started: Instant::now(), paused: None, paused_for: Duration::ZERO, secs: 0, screen: (sw, sh), drag: None, glide: None })
    }

    pub fn event(&mut self, conn: &impl Connection, ev: &Event) -> Res<PillEvent> {
        let (w, h) = (self.win.canvas.w as i32, self.win.canvas.h as i32);
        match ev {
            Event::Expose(e) if e.window == self.win.id => self.win.draw(conn)?,
            Event::ButtonPress(e) if e.event == self.win.id && e.detail == 1 => {
                self.glide = None;
                let (root, from) = ((e.root_x as i32, e.root_y as i32), (self.win.x, self.win.y));
                self.drag = Some(Drag { root, from, press_x: e.event_x, moved: false });
            }
            Event::MotionNotify(e) if e.event == self.win.id => {
                if let Some(d) = &mut self.drag {
                    let (dx, dy) = (e.root_x as i32 - d.root.0, e.root_y as i32 - d.root.1);
                    d.moved |= dx.abs() + dy.abs() > 4;
                    if d.moved {
                        let (x, y) = ((d.from.0 + dx).clamp(0, self.screen.0 - w), (d.from.1 + dy).clamp(0, self.screen.1 - h));
                        self.win.move_to(conn, x, y)?;
                    }
                }
            }
            Event::ButtonRelease(e) if e.event == self.win.id && e.detail == 1 => match self.drag.take() {
                Some(d) if d.moved => self.glide = Some((Instant::now(), (self.win.x, self.win.y), self.nearest_edge())),
                Some(d) => {
                    return Ok(match pill_slot(self.scale, d.press_x, self.div) {
                        1 => PillEvent::TogglePause,
                        2 => PillEvent::Stop,
                        _ => PillEvent::None,
                    });
                }
                None => {}
            },
            _ => {}
        }
        Ok(PillEvent::None)
    }

    /// Where the pill rests if let go now: against the closest screen edge.
    fn nearest_edge(&self) -> (i32, i32) {
        let (w, h) = (self.win.canvas.w as i32, self.win.canvas.h as i32);
        let (sw, sh) = self.screen;
        let (cx, cy) = (self.win.x + w / 2, self.win.y + h / 2);
        let e = (EDGE as f32 * self.scale).round() as i32;
        let (x, y) = (self.win.x.clamp(e, sw - w - e), self.win.y.clamp(e, sh - h - e));
        let spots = [(cx, (e, y)), (sw - cx, (sw - w - e, y)), (cy, (x, e)), (sh - cy, (x, sh - h - e))];
        spots.into_iter().min_by_key(|s| s.0).unwrap().1
    }

    /// Whether a glide is under way (then it needs a step every tick).
    pub fn gliding(&self) -> bool {
        self.glide.is_some()
    }

    /// Step the glide to the edge (ease-out, a quarter second).
    pub fn animate(&mut self, conn: &impl Connection) -> Res<()> {
        let Some((t0, (x0, y0), (x1, y1))) = self.glide else { return Ok(()) };
        let t = (t0.elapsed().as_secs_f32() / 0.25).min(1.0);
        let e = 1.0 - (1.0 - t).powi(3);
        let lerp = |a: i32, b: i32| a + ((b - a) as f32 * e).round() as i32;
        self.win.move_to(conn, lerp(x0, x1), lerp(y0, y1))?;
        if t >= 1.0 {
            self.glide = None;
        }
        Ok(())
    }

    pub fn set_paused(&mut self, conn: &impl Connection, paused: bool) -> Res<()> {
        match (paused, self.paused) {
            (true, None) => self.paused = Some(Instant::now()),
            (false, Some(t)) => (self.paused, self.paused_for) = (None, self.paused_for + t.elapsed()),
            _ => {}
        }
        self.win.redraw(conn, pill(paused, self.secs, self.fonts, self.scale))
    }

    /// Keep the elapsed time current: redraws only when the shown second
    /// changes (once a second, never while paused), and says whether it did.
    pub fn tick(&mut self, conn: &impl Connection) -> Res<bool> {
        let secs = (self.paused.unwrap_or_else(Instant::now) - self.started - self.paused_for).as_secs();
        if secs == self.secs {
            return Ok(false);
        }
        self.secs = secs;
        self.win.redraw(conn, pill(self.paused.is_some(), secs, self.fonts, self.scale))?;
        Ok(true)
    }
}

/// Offscreen renders of every surface, for design review:
/// `SCREENREC_PREVIEW=<dir> cargo test preview -- --ignored`.
#[cfg(test)]
mod preview {
    use super::*;
    use crate::i18n::{self, Lang};

    const SCALE: f32 = 1.25; // the 120 dpi screen

    /// Save premultiplied ARGB pixels over an opaque `bg` as an RGB PNG.
    fn save(dir: &str, name: &str, w: usize, h: usize, px: &[u32], bg: [u8; 3]) {
        let rgb: Vec<u8> = px
            .iter()
            .flat_map(|&p| {
                let a = 255 - (p >> 24);
                [(p >> 16 & 255, bg[0]), (p >> 8 & 255, bg[1]), (p & 255, bg[2])].map(|(c, b)| (c + (b as u32 * a + 127) / 255).min(255) as u8)
            })
            .collect();
        let mut png = png::Encoder::new(std::fs::File::create(format!("{dir}/{name}.png")).unwrap(), w as u32, h as u32);
        png.set_color(png::ColorType::Rgb);
        let mut wr = png.write_header().unwrap();
        wr.write_image_data(&rgb).unwrap();
    }

    /// Blend canvas `c` onto `dst` (premultiplied ARGB, `dw` wide) at (x, y).
    fn over(dst: &mut [u32], dw: usize, c: &Canvas, x: usize, y: usize) {
        for (cy, row) in c.px.chunks_exact(c.w).enumerate() {
            for (cx, &p) in row.iter().enumerate() {
                let d = &mut dst[(y + cy) * dw + x + cx];
                let a = 255 - (p >> 24);
                let ch = |sh: u32| ((p >> sh & 255) + ((*d >> sh & 255) * a + 127) / 255).min(255);
                *d = 255 << 24 | ch(16) << 16 | ch(8) << 8 | ch(0);
            }
        }
    }

    /// A made-up 1920×1080 desktop: wallpaper and two windows.
    fn desktop() -> Canvas {
        let (w, h) = (1920, 1080);
        let mut c = Canvas::new(w, h, 1.0);
        for y in 0..h {
            for x in 0..w {
                let (t, u) = (x as f32 / w as f32, y as f32 / h as f32);
                let ch = |a: f32, b: f32| ((a + (b - a) * (t * 0.6 + u * 0.4)) * 255.0) as u32;
                c.px[y * w + x] = 255 << 24 | ch(0.17, 0.85) << 16 | ch(0.12, 0.42) << 8 | ch(0.33, 0.25);
            }
        }
        for (x0, y0, x1, y1, bar) in [(180.0, 120.0, 1100.0, 760.0, hex(0xEBEBEB)), (820.0, 300.0, 1700.0, 900.0, hex(0x333333))] {
            c.paint((0.0, 0.0, 0.0, 0.35), rrect(x0 - 6.0, y0 - 2.0, x1 + 6.0, y1 + 12.0, 16.0));
            c.paint(if bar.0 > 0.5 { WHITE } else { hex(0x242424) }, rrect(x0, y0, x1, y1, 10.0));
            c.paint(bar, rrect(x0, y0, x1, y0 + 46.0, 10.0));
            for i in 0..8 {
                let y = y0 + 90.0 + i as f32 * 48.0;
                let col = if bar.0 > 0.5 { hex(0xBFBFBF) } else { hex(0x595959) };
                c.paint(col, rrect(x0 + 40.0, y, x0 + 40.0 + (x1 - x0 - 80.0) * (0.4 + 0.07 * (i % 5) as f32), y + 14.0, 7.0));
            }
        }
        c
    }

    type Fonts<'a> = (Option<&'a FontVec>, Option<&'a FontVec>);

    /// `s` with Área's switch on.
    fn ocr(mut s: PanelState) -> PanelState {
        s.ocr = true;
        s.settle();
        s
    }

    fn set_state<'a>(fonts: Fonts<'a>, cjk: Option<&'a FontVec>, scale: f32) -> SettingsState<'a> {
        let mut s = SettingsState::new(fonts, cjk, scale);
        (s.output, s.mic, s.mp4, s.gpu, s.gpu_found, s.clip, s.shortcut) = (1, true, false, true, true, true, "Ctrl+Shift+S".into());
        s.settle();
        s
    }

    /// The overlay over the frozen desktop, as premultiplied pixels.
    fn overlay(frozen: &[u8], (sw, sh): (usize, usize), area: Rect, handles: bool) -> Vec<u32> {
        crate::select::preview(frozen.to_vec(), sw, sh, Some(area), handles).as_chunks::<4>().0.iter().map(|p| 255 << 24 | u32::from_le_bytes([p[0], p[1], p[2], 0])).collect()
    }

    #[test]
    #[ignore = "writes PNGs for design review"]
    fn render_surfaces() {
        let dir = std::env::var("SCREENREC_PREVIEW").unwrap_or_else(|_| "/tmp/screenrec-preview".into());
        std::fs::create_dir_all(&dir).unwrap();
        let bg = [0x30, 0x2a, 0x3a];
        let desk = desktop();
        let frozen: Vec<u8> = desk.px.iter().flat_map(|p| p.to_le_bytes()).collect();
        let (sw, sh) = (desk.w, desk.h);
        let (latin, bold, ja) = (load_font(false), load_bold(), load_font(true));
        for (lang, tag) in [(Lang::En, "en"), (Lang::Es, "es"), (Lang::Ja, "ja")] {
            i18n::set(lang);
            let (fonts, cjk) = if lang == Lang::Ja { ((ja.as_ref(), ja.as_ref()), None) } else { ((latin.as_ref(), bold.as_ref()), ja.as_ref()) };
            let font = fonts.0;
            let panel_state = |mode, record, hover, open, focus| {
                let mut s = PanelState::new(mode, record, fonts, SCALE);
                (s.hover, s.settings_open, s.focus) = (hover, open, focus);
                s.settle();
                s
            };
            let mut mid = panel_state(Mode::Screen, true, None, false, None);
            (mid.tw.mode, mid.tw.record) = (Tween::io(0.5), Tween::io(0.5));
            // Halfway from Área to Pantalla: the switch shrinking back into the dot.
            let mut morph = ocr(panel_state(Mode::Selection, false, None, false, None));
            morph.tw.mode = Tween::io(0.5);
            let panels = [
                ("shot", panel_state(Mode::Selection, false, None, false, None)),
                ("rec-hover", panel_state(Mode::Window, true, Some(Hit::Shutter), false, None)),
                ("screen-gear", panel_state(Mode::Screen, false, Some(Hit::Mode(Mode::Window)), true, None)),
                ("focus", panel_state(Mode::Selection, true, None, false, Some(Hit::Mode(Mode::Screen)))),
                ("focus-shutter", panel_state(Mode::Screen, false, None, false, Some(Hit::Shutter))),
                ("focus-window", panel_state(Mode::Screen, false, Some(Hit::Close), false, Some(Hit::Mode(Mode::Window)))),
                ("mid", mid),
                ("area-ocr-on", ocr(panel_state(Mode::Selection, false, None, false, None))),
                ("area-rec-ocr-on", ocr(panel_state(Mode::Selection, true, None, false, None))),
                ("area-ocr-hover", panel_state(Mode::Selection, false, Some(Hit::Ocr), false, None)),
                ("focus-ocr", panel_state(Mode::Selection, false, None, false, Some(Hit::Ocr))),
                ("focus-ocr-on", ocr(panel_state(Mode::Selection, false, None, false, Some(Hit::Ocr)))),
                ("screen-ocr-on", ocr(panel_state(Mode::Screen, false, None, false, None))),
                ("window-ocr-on", ocr(panel_state(Mode::Window, false, None, false, None))),
                ("morph", morph),
            ];
            // Every mode word fits its cell with room to spare.
            for m in MODES {
                let (word, px, track) = caps(&mode_name(m), 13.0);
                let w = spaced_width(fonts.1.or(font).unwrap(), &word, px, track, false);
                assert!(w <= CELL.0 - 16.0, "{word}: {w} of {}", CELL.0);
            }
            for (name, s) in &panels {
                let c = panel(s);
                save(&dir, &format!("panel-{name}-{tag}"), c.w, c.h, &c.px, bg);
            }
            // The languages' own settings views.
            let c = settings(&set_state(fonts, cjk, SCALE));
            save(&dir, &format!("settings-default-{tag}"), c.w, c.h, &c.px, bg);
            let mut listening = set_state(fonts, if tag == "en" { None } else { ja.as_ref() }, SCALE);
            (listening.gpu_found, listening.capturing, listening.hover, listening.pointer) = (false, true, Some(SetHit::Pointer), false);
            listening.settle();
            let c = settings(&listening);
            save(&dir, &format!("settings-listening-{tag}"), c.w, c.h, &c.px, bg);
            let mut focused = set_state(fonts, if tag == "es" { ja.as_ref() } else { None }, SCALE);
            (focused.focus, focused.hover) = (Some(SetHit::Lang(1)), Some(SetHit::VideoFormat(1)));
            focused.settle();
            let c = settings(&focused);
            save(&dir, &format!("settings-focus-{tag}"), c.w, c.h, &c.px, bg);
            // The clipboard row: the image, the text (Área's switch on), off limits on video; its focus ring.
            for (name, record, text, focus) in [("image", false, false, None), ("text", false, true, None), ("video", true, true, None), ("focus", false, false, Some(SetHit::Clip))] {
                let mut s = set_state(fonts, cjk, SCALE);
                (s.record, s.text, s.focus) = (record, text, focus);
                s.settle();
                let c = settings(&s);
                save(&dir, &format!("settings-clip-{name}-{tag}"), c.w, c.h, &c.px, bg);
            }

            // The whole launcher over the frozen desktop, as the user sees it.
            let ((px, py), (mx, my)) = place(sw as i32, sh as i32, SCALE);
            let gap = (14.0 * SCALE).round() as i32;
            let with_badge = |full: &mut Vec<u32>, r: Rect, name: Option<&str>| {
                let bc = badge(r.2 - r.0, r.3 - r.1, name, font, SCALE);
                let (bx, by) = badge_pos(r, (bc.w as i32, bc.h as i32), (sw as i32, sh as i32), gap, panel_rect(sw as i32, sh as i32, SCALE));
                over(full, sw, &bc, bx as usize, by as usize);
            };
            let sel = (420, 260, 1240, 720);
            let mut full = overlay(&frozen, (sw, sh), sel, true);
            with_badge(&mut full, sel, None);
            over(&mut full, sw, &panel(&panels[0].1), px as usize, py as usize);
            save(&dir, &format!("launcher-{tag}"), sw, sh, &full, bg);
            over(&mut full, sw, &settings(&set_state(fonts, cjk, SCALE)), mx as usize, my as usize);
            save(&dir, &format!("launcher-settings-{tag}"), sw, sh, &full, bg);
            // Área's switch with its tooltip: off, on (a .txt, or the clipboard), on while recording.
            for (name, on, record, clip) in [("off", false, false, false), ("on", true, false, false), ("clip", true, false, true), ("video", true, true, false)] {
                let sel = (420, 260, 1240, 720);
                let mut full = overlay(&frozen, (sw, sh), sel, true);
                with_badge(&mut full, sel, None);
                let mut s = panel_state(Mode::Selection, record, Some(Hit::Ocr), false, None);
                s.ocr = on;
                s.settle();
                over(&mut full, sw, &panel(&s), px as usize, py as usize);
                let t = tooltip(&ocr_tip(on, record, clip), font, SCALE);
                let (tx, ty) = tooltip_pos(sw as i32, sh as i32, SCALE, (t.w as i32, t.h as i32));
                over(&mut full, sw, &t, tx as usize, ty as usize);
                let (x0, y0, cw) = (560, 740, 800);
                let crop: Vec<u32> = full.chunks_exact(sw).skip(y0).flat_map(|r| r[x0..x0 + cw].to_vec()).collect();
                save(&dir, &format!("launcher-tip-{name}-{tag}"), cw, sh - y0, &crop, bg);
            }
            // Screen mode: no badge, no switch.
            let mut full = overlay(&frozen, (sw, sh), (0, 0, sw as i32, sh as i32), false);
            over(&mut full, sw, &panel(&panel_state(Mode::Screen, false, None, false, None)), px as usize, py as usize);
            save(&dir, &format!("launcher-screen-{tag}"), sw, sh, &full, bg);
            // A tall selection: the badge goes inside it.
            let sel = (300, 200, 1500, 1060);
            let mut full = overlay(&frozen, (sw, sh), sel, true);
            with_badge(&mut full, sel, None);
            over(&mut full, sw, &panel(&panels[0].1), px as usize, py as usize);
            save(&dir, &format!("launcher-badge-inside-{tag}"), sw, sh, &full, bg);
            // Window mode: the window framed, its name on the badge.
            let win = (820, 300, 1700, 900);
            let mut full = overlay(&frozen, (sw, sh), win, false);
            with_badge(&mut full, win, Some("Firefox Web Browser"));
            over(&mut full, sw, &panel(&panel_state(Mode::Window, true, None, false, None)), px as usize, py as usize);
            save(&dir, &format!("launcher-window-{tag}"), sw, sh, &full, bg);
            // A selection against the screen's corner: the brackets stay on screen.
            let sel = (0, 0, 900, 500);
            let full = overlay(&frozen, (sw, sh), sel, true);
            save(&dir, &format!("overlay-corner-{tag}"), 1000, 600, &full.chunks_exact(sw).take(600).flat_map(|r| r[..1000].to_vec()).collect::<Vec<_>>(), bg);
            // The pill over light and dark backgrounds.
            for (name, paused, secs) in [("recording", false, 42), ("paused", true, 725), ("hour", false, 3723)] {
                let c = pill(paused, secs, fonts, SCALE);
                save(&dir, &format!("pill-{name}-light-{tag}"), c.w, c.h, &c.px, [0xf2, 0xf2, 0xf2]);
                save(&dir, &format!("pill-{name}-dark-{tag}"), c.w, c.h, &c.px, [0x24, 0x24, 0x24]);
            }
            // The badge alone, for the selection and for a window.
            for (w, h, name) in [(1280, 720, None), (64, 64, None), (1920, 1080, None), (880, 600, Some("Firefox Web Browser"))] {
                let c = badge(w, h, name, font, SCALE);
                save(&dir, &format!("badge-{w}x{h}-{tag}"), c.w, c.h, &c.px, bg);
            }
        }
        // At 1x too: the panel at rest and with the focus by the close button, and the settings.
        for (lang, tag) in [(Lang::En, "en"), (Lang::Es, "es"), (Lang::Ja, "ja")] {
            i18n::set(lang);
            let fonts = if lang == Lang::Ja { (ja.as_ref(), ja.as_ref()) } else { (latin.as_ref(), bold.as_ref()) };
            for (name, focus) in [("shot", None), ("focus-window", Some(Hit::Mode(Mode::Window)))] {
                let mut s = PanelState::new(Mode::Selection, false, fonts, 1.0);
                s.focus = focus;
                s.settle();
                let c = panel(&s);
                save(&dir, &format!("panel-{name}-{tag}-1x"), c.w, c.h, &c.px, bg);
            }
            let c = settings(&set_state(fonts, if lang == Lang::Ja { None } else { ja.as_ref() }, 1.0));
            save(&dir, &format!("settings-default-{tag}-1x"), c.w, c.h, &c.px, bg);
        }
        i18n::set(Lang::En);
    }

    /// `cargo test --release timing -- --ignored --nocapture`
    #[test]
    #[ignore = "timing"]
    fn timing() {
        i18n::set(Lang::En);
        let (font, bold) = (load_font(false), load_bold());
        let mut p = PanelState::new(Mode::Selection, false, (font.as_ref(), bold.as_ref()), SCALE);
        p.settle();
        let mut s = set_state((font.as_ref(), bold.as_ref()), None, SCALE);
        s.settle();
        let time = |f: &dyn Fn() -> Canvas| {
            f();
            (0..10).map(|_| {
                let t0 = Instant::now();
                for _ in 0..30 {
                    std::hint::black_box(f());
                }
                t0.elapsed().as_secs_f64() * 1000.0 / 30.0
            }).fold(f64::MAX, f64::min) // best of ten: the machine is shared
        };
        println!("panel {:.2} ms, settings {:.2} ms", time(&|| panel(&p)), time(&|| settings(&s)));
        // What a fade drawn here would cost a frame: the 1080p overlay redrawn, and the panel at partial alpha (no card cache).
        let frozen = vec![128u8; 1920 * 1080 * 4];
        let t0 = Instant::now();
        for _ in 0..10 {
            std::hint::black_box(crate::select::preview(frozen.clone(), 1920, 1080, Some((420, 260, 1240, 720)), true));
        }
        let overlay = t0.elapsed().as_secs_f64() * 100.0;
        let t0 = Instant::now();
        for _ in 0..10 {
            let mut c = Canvas::new(PW, PH, SCALE);
            c.alpha = 0.5;
            card(&mut c, BODY.0, BODY.1, BODY_R);
            std::hint::black_box(c);
        }
        println!("overlay 1080p {overlay:.2} ms, panel card at half alpha {:.2} ms", t0.elapsed().as_secs_f64() * 100.0);
    }

    #[test]
    fn settled_cards_are_copied_exactly() {
        for scale in [1.0, 1.25] {
            let mut p = PanelState::new(Mode::Selection, false, (None, None), scale);
            p.settle();
            let s = set_state((None, None), None, scale);
            // The first frame paints each card and keeps it, the second copies it.
            assert!(panel(&p).px == panel(&p).px && settings(&s).px == settings(&s).px);
            CARDS.with_borrow(|cache| assert_eq!(cache.iter().filter(|e| e.0.5 == scale.to_bits()).count(), 2));
        }
    }

    #[test]
    fn fit_ellipsises() {
        let Some(f) = load_font(false) else { return };
        let t = Canvas::fit(&f, "Firefox Web Browser", 12.0, 96.0);
        assert!(t.ends_with('…') && Canvas::width(&f, &t, 12.0) <= 96.0, "{t}");
        assert_eq!(Canvas::fit(&f, "Firefox", 12.0, 96.0), "Firefox");
    }

    #[test]
    fn pill_text_and_slots() {
        assert_eq!((clock(42), clock(725), clock(3723)), ("0:42".into(), "12:05".into(), "1:02:03".into()));
        assert_eq!([0, 99, 100, 129, 130, 160].map(|x| pill_slot(1.0, x, 100.0)), [0, 0, 1, 1, 2, 2]);
        assert_eq!((pill_slot(1.25, 124, 100.0), pill_slot(1.25, 126, 100.0)), (0, 1));
        // Pausing changes the word, never where the buttons are.
        let f = (load_font(false), load_bold());
        let (_, div, w) = pill_geo(f.0.as_ref(), f.1.as_ref());
        assert_eq!(pill(true, 5, (f.0.as_ref(), f.1.as_ref()), 1.0).w, w as usize);
        assert!(div + 50.0 < w);
    }

    fn bounds(g: Geo) -> (f32, f32, f32, f32) {
        g.shape().b
    }

    #[test]
    fn hits_match_looks() {
        for s in [1.0, 1.25] {
            let centre = |(x0, y0, x1, y1): (f32, f32, f32, f32)| ((((x0 + x1) / 2.0 + M) * s) as i16, (((y0 + y1) / 2.0 + M) * s) as i16);
            for h in PANEL_ORDER {
                let (x, y) = centre(bounds(shape_of(h)));
                assert_eq!(panel_hit(s, x, y, true), Some(h));
            }
            // Out of Área mode its switch isn't there: the spot is Área's cell.
            let (x, y) = centre(bounds(shape_of(Hit::Ocr)));
            assert_eq!(panel_hit(s, x, y, false), Some(Hit::Mode(Mode::Selection)));
            for h in SETTINGS_ORDER {
                let (x, y) = centre(bounds(set_shape(h)));
                assert_eq!(settings_hit(s, x, y), Some(h));
            }
        }
        assert_eq!(panel_hit(1.0, 5, 5, true), None);
    }

    #[test]
    fn focus_steps_wrap_and_skip() {
        let o = [1, 2, 3, 4];
        assert_eq!([step(&o, 1, false, |_| false), step(&o, 4, false, |_| false), step(&o, 1, true, |_| false), step(&o, 3, true, |_| false)], [2, 1, 4, 2]);
        assert_eq!([step(&o, 1, false, |h| h == 2), step(&o, 3, true, |h| h == 2), step(&o, 4, false, |h| h == 1)], [3, 1, 2]);
        let no_gpu = |h| h == SetHit::Gpu;
        assert_eq!(step(&SETTINGS_ORDER, SetHit::VideoFormat(1), false, no_gpu), SetHit::ImageFormat(0));
        assert_eq!(step(&SETTINGS_ORDER, SetHit::ImageFormat(0), true, no_gpu), SetHit::VideoFormat(1));
        assert_eq!(step(&SETTINGS_ORDER, SetHit::Close, false, no_gpu), SetHit::Output(0));
        assert_eq!(step(&PANEL_ORDER, Hit::Close, false, |_| false), Hit::Mode(Mode::Selection));
        assert_eq!(step(&PANEL_ORDER, Hit::Mode(Mode::Selection), true, |_| false), Hit::Close);
        assert_eq!(step(&PANEL_ORDER, Hit::Mode(Mode::Window), false, |_| false), Hit::Ocr); // right after the mode words
        assert_eq!(step(&PANEL_ORDER, Hit::Mode(Mode::Window), false, |h| h == Hit::Ocr), Hit::Shot);
        assert_eq!(step(&SETTINGS_ORDER, SetHit::ImageFormat(1), false, |h| h == SetHit::Clip), SetHit::Pointer);
    }

    #[test]
    fn margins_are_not_body() {
        for s in [1.0, 1.25] {
            let d = |v: f32| (v * s) as i16;
            assert!(!panel_body_has(s, 1, 1) && !panel_body_has(s, d(M + BODY.0 + 20.0), d(M + BODY.1 + 20.0)));
            assert!(panel_body_has(s, d(M + 30.0), d(M + 30.0)) && panel_body_has(s, d(M + 280.0), d(M + BODY.1 - 11.0)));
            assert!(panel_body_has(s, d(M + BODY.0 + 4.0), d(M - 4.0)), "the close button overhangs the card");
            assert!(!settings_body_has(s, 1, 1) && !settings_body_has(s, d(M + SBODY.0 + 20.0), d(M + SBODY.1 + 20.0)));
            assert!(settings_body_has(s, d(M + 170.0), d(M + 240.0)) && settings_body_has(s, d(M + 30.0), d(M + 30.0)));
        }
    }

    #[test]
    fn badge_placement() {
        let (b, scr, none) = ((90, 24), (1920, 1080), (0, 0, 0, 0));
        assert_eq!(badge_pos((420, 260, 1240, 720), b, scr, 8, none), (420, 728)); // below, left-aligned
        assert_eq!(badge_pos((420, 260, 1240, 1070), b, scr, 8, none), (428, 268)); // no room: inside
        assert_eq!(badge_pos((1900, 100, 1919, 200), b, scr, 8, none), (1830, 208)); // clamped right
        assert_eq!(badge_pos((-30, 100, 200, 200), b, scr, 8, none), (0, 208)); // and left
        assert_eq!(badge_pos((0, 0, 1920, 1080), (200, 24), (300, 100), 8, none), (8, 8)); // flipped, on a small screen
        let panel = panel_rect(1920, 1080, 1.25);
        assert_eq!(badge_pos((820, 300, 1700, 900), b, scr, 8, panel), (828, 308)); // not behind the panel
        assert_eq!(badge_pos((1300, 300, 1700, 900), b, scr, 8, panel), (1300, 908)); // beside it is fine
    }

    #[test]
    fn badge_is_just_big_enough() {
        let f = load_font(false);
        let c = badge(1280, 720, None, f.as_ref(), 1.25);
        assert_eq!(c.h, 30);
        assert!(c.w > 60 && c.w < 120 && c.px.iter().all(|p| p >> 24 > 0 || *p == 0));
        // Same digit count, same width: dragging doesn't make it jump.
        assert_eq!(badge(1111, 111, None, f.as_ref(), 1.25).w, badge(1080, 720, None, f.as_ref(), 1.25).w);
    }

    #[test]
    fn tween_eases_and_retargets() {
        let mut t = Tween::new(0.0);
        t.go(1.0, 100.0);
        assert!(t.busy() && t.get() < 0.5);
        t.settle();
        assert_eq!((t.get(), t.busy()), (1.0, false));
        assert_eq!(Tween::io(0.5).get(), 0.5);
    }

    #[test]
    fn pill_alpha_budget() {
        let (f, b) = (load_font(false), load_bold());
        let c = pill(false, 3723, (f.as_ref(), b.as_ref()), SCALE);
        let max = c.px.iter().map(|p| p >> 24).max().unwrap();
        assert!(max <= 230, "{max}"); // 0.9, the dot
        let paused = pill(true, 12, (f.as_ref(), b.as_ref()), 1.0);
        assert!(paused.px.iter().all(|p| p >> 24 <= 205), "only the dot goes past 0.8");
    }
}

/// Live check on a real compositor: the pill, redrawn every 150 ms (the
/// timer's worst case, times seven), never shows up in captured frames.
/// `cargo test --release pill_never_in_frames -- --ignored --nocapture`;
/// puts two small windows in the bottom-right corner for ~3 s, no grabs.
#[cfg(test)]
mod live {
    use super::*;

    #[test]
    #[ignore = "needs an X display with a compositor"]
    fn pill_never_in_frames() {
        let mut cap = Capture::new().unwrap();
        let (font, bold, scale) = (load_font(false), load_bold(), 1.25);
        let (sw, sh) = (cap.sw as i32, cap.sh as i32);
        // An opaque backdrop with known pixels under the pill.
        let (bw, bh) = (320usize, 110usize);
        let mut back = Canvas::new(bw, bh, 1.0);
        for (i, p) in back.px.iter_mut().enumerate() {
            let (x, y) = ((i % bw) as u32, (i / bw) as u32);
            *p = 255 << 24 | (x * 255 / bw as u32) << 16 | (y * 255 / bh as u32) << 8 | if (x / 6 + y / 6) % 2 == 0 { 40 } else { 220 };
        }
        let (bx, by) = (sw - bw as i32, sh - bh as i32);
        let backdrop = Win::new(&cap, bx, by, back, EventMask::EXPOSURE).unwrap();
        backdrop.show(&cap.conn).unwrap();
        let fonts = (font.as_ref(), bold.as_ref());
        let mut pill = Pill::new(&cap, fonts, scale).unwrap();
        cap.overlay = Some(pill.win.sprite());
        cap.draw_pointer = false;
        cap.track_changes().unwrap();
        crate::settle(&mut cap, None).unwrap();
        cap.set_region(bx, by, bw as i32, bh as i32);
        let expect = |x: i32, y: i32| backdrop.canvas.px[((y - by) as usize) * bw + (x - bx) as usize];
        let (mut frames, mut worst, mut bad_frames) = (0, 0u32, 0);
        let t0 = Instant::now();
        let mut next = t0;
        let mut secs = 0;
        while t0.elapsed() < Duration::from_secs(3) {
            if Instant::now() >= next {
                secs += 1;
                pill.win.redraw(&cap.conn, pill_canvas(secs, fonts, scale)).unwrap();
                cap.set_overlay(pill.win.sprite());
                next += Duration::from_millis(150);
            }
            cap.wait(Duration::from_millis(2)).unwrap();
            let rows = (0, bh as i32);
            cap.grab(rows).unwrap();
            frames += 1;
            let (v, f) = (cap.view, cap.frame());
            let (px, py, pw, ph) = (pill.win.x, pill.win.y, pill.win.canvas.w as i32, pill.win.canvas.h as i32);
            let mut bad = 0;
            for y in py..py + ph {
                for x in px..px + pw {
                    let i = (((y - v.y0) as usize) * v.w + (x - v.x0) as usize) * 4;
                    let e = expect(x, y).to_le_bytes();
                    let d = (0..3).map(|k| (f[i + k] as i32 - e[k] as i32).unsigned_abs()).max().unwrap();
                    worst = worst.max(d);
                    bad += (d > 8) as u32;
                }
            }
            bad_frames += (bad > 0) as u32;
        }
        cap.conn.unmap_window(pill.win.id).unwrap();
        cap.conn.unmap_window(backdrop.id).unwrap();
        cap.conn.flush().unwrap();
        eprintln!("{frames} frames, {secs} pill redraws: {bad_frames} frames with pill pixels left, worst error {worst} levels");
        assert_eq!(bad_frames, 0);
    }

    fn pill_canvas(secs: u64, fonts: (Option<&FontVec>, Option<&FontVec>), scale: f32) -> Canvas {
        pill(false, secs * 7, fonts, scale) // a new look every time
    }
}
