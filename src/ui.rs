//! The GUI, drawn here pixel by pixel (antialiased signed-distance shapes,
//! text via ab_glyph) into ARGB override-redirect windows: the launcher
//! panel, modelled on GNOME 42's screenshot UI, and the recording pill.
//! Knowing every pixel we put on screen is what lets capture.rs take the
//! pill back out of the video.

use crate::Res;
use crate::capture::{Capture, Sprite};
use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use std::time::Instant;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as _;

/// Premultiplied ARGB pixels.
pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
}

type Rgba = (f32, f32, f32, f32);
const fn gray(v: f32) -> Rgba {
    (v, v, v, 1.0)
}
const WHITE: Rgba = gray(1.0);
const RED: Rgba = (0.88, 0.11, 0.14, 1.0); // GNOME's record red, #e01b24
const PANEL: Rgba = gray(0.141); // #242424
const LIT: Rgba = gray(0.227); // selected button, #3a3a3a
const HOVER: Rgba = gray(0.19);

/// A shape: its bounding box and signed distance (negative inside).
struct Shape<F: Fn(f32, f32) -> f32> {
    b: (f32, f32, f32, f32),
    d: F,
}

fn circle(cx: f32, cy: f32, r: f32) -> Shape<impl Fn(f32, f32) -> f32> {
    Shape { b: (cx - r, cy - r, cx + r, cy + r), d: move |x: f32, y: f32| (x - cx).hypot(y - cy) - r }
}

fn rrect(x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> Shape<impl Fn(f32, f32) -> f32> {
    let d = move |x: f32, y: f32| {
        let qx = (x - (x0 + x1) / 2.0).abs() - (x1 - x0) / 2.0 + r;
        let qy = (y - (y0 + y1) / 2.0).abs() - (y1 - y0) / 2.0 + r;
        qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r
    };
    Shape { b: (x0, y0, x1, y1), d }
}

/// Segment from (ax, ay) to (bx, by), `r` thick on each side.
fn line(ax: f32, ay: f32, bx: f32, by: f32, r: f32) -> Shape<impl Fn(f32, f32) -> f32> {
    let d = move |x: f32, y: f32| {
        let (dx, dy) = (bx - ax, by - ay);
        let t = (((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
        (x - ax - t * dx).hypot(y - ay - t * dy) - r
    };
    Shape { b: (ax.min(bx) - r, ay.min(by) - r, ax.max(bx) + r, ay.max(by) + r), d }
}

/// The outline of `s`, `w` wide.
fn stroke<F: Fn(f32, f32) -> f32>(s: Shape<F>, w: f32) -> Shape<impl Fn(f32, f32) -> f32> {
    let (x0, y0, x1, y1) = s.b;
    Shape { b: (x0 - w, y0 - w, x1 + w, y1 + w), d: move |x, y| (s.d)(x, y).abs() - w / 2.0 }
}

impl Canvas {
    fn new(w: usize, h: usize) -> Self {
        Canvas { w, h, px: vec![0; w * h] }
    }

    /// Paint `color` over pixel (x, y) with coverage `cov`.
    fn blend(&mut self, x: i32, y: i32, (r, g, b, a): Rgba, cov: f32) {
        let cov = cov.clamp(0.0, 1.0) * a;
        if cov <= 0.0 || x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return;
        }
        let p = &mut self.px[y as usize * self.w + x as usize];
        let ch = |sh: u32, src: f32| (src * cov * 255.0 + (*p >> sh & 255) as f32 * (1.0 - cov)).round() as u32;
        *p = ch(24, 1.0) << 24 | ch(16, r) << 16 | ch(8, g) << 8 | ch(0, b);
    }

    fn paint<F: Fn(f32, f32) -> f32>(&mut self, color: Rgba, s: Shape<F>) {
        let (x0, y0, x1, y1) = s.b;
        for y in y0.floor() as i32 - 1..=y1.ceil() as i32 {
            for x in x0.floor() as i32 - 1..=x1.ceil() as i32 {
                self.blend(x, y, color, 0.5 - (s.d)(x as f32 + 0.5, y as f32 + 0.5));
            }
        }
    }

    /// Any polygon (concave too), 4×4 supersampled.
    fn poly(&mut self, color: Rgba, p: &[(f32, f32)]) {
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
                self.blend(x, y, color, n as f32 / 16.0);
            }
        }
    }

    /// Width of `s` at em size `px`.
    fn width(font: &FontVec, s: &str, px: f32) -> f32 {
        let sf = font.as_scaled(font.pt_to_px_scale(px * 0.75).unwrap_or(PxScale::from(px)));
        s.chars().map(|ch| sf.h_advance(font.glyph_id(ch))).sum()
    }

    /// `s` cut to at most `max` px wide at em size `px`, with an ellipsis.
    fn fit(font: &FontVec, s: &str, px: f32, max: f32) -> String {
        let mut t: String = s.into();
        while !t.is_empty() && Self::width(font, &t, px) > max {
            t.pop();
            while t.ends_with(' ') {
                t.pop();
            }
            if Self::width(font, &format!("{t}…"), px) <= max {
                return format!("{t}…");
            }
        }
        t
    }

    /// `s` on `baseline`, `px` em size; `align` 0 puts its left end at x, 0.5 its centre.
    #[allow(clippy::too_many_arguments)]
    fn text(&mut self, font: &FontVec, s: &str, px: f32, x: f32, baseline: f32, align: f32, color: Rgba) {
        let scale = font.pt_to_px_scale(px * 0.75).unwrap_or(PxScale::from(px));
        let sf = font.as_scaled(scale);
        let mut x = x - s.chars().map(|ch| sf.h_advance(font.glyph_id(ch))).sum::<f32>() * align;
        for ch in s.chars() {
            let id = font.glyph_id(ch);
            if let Some(g) = font.outline_glyph(id.with_scale_and_position(scale, ab_glyph::point(x, baseline))) {
                let b = g.px_bounds();
                g.draw(|gx, gy, cov| self.blend(b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32, color, cov));
            }
            x += sf.h_advance(id);
        }
    }
}

/// The desktop's UI font (Ubuntu here), or a Japanese one; without one the
/// panel just has no labels.
pub fn load_font(ja: bool) -> Option<FontVec> {
    let name = if ja { "sans-serif:lang=ja" } else { "Ubuntu" };
    let out = std::process::Command::new("fc-match").args(["-f", "%{file}", name]).output().ok()?;
    FontVec::try_from_vec(std::fs::read(String::from_utf8(out.stdout).ok()?).ok()?).ok()
}

// Icons, line art in GNOME's symbolic style. `bg` is what's behind them, for cut-outs.

fn icon_selection(c: &mut Canvas, cx: f32, cy: f32, col: Rgba) {
    let (x0, y0, x1, y1) = (cx - 13.0, cy - 10.0, cx + 13.0, cy + 10.0);
    for i in 0..4 {
        let t = 5.5 + i as f32 * 5.0; // dashes between the corner dots
        if t + 2.5 < 26.0 {
            c.paint(col, line(x0 + t, y0, x0 + t + 2.5, y0, 1.0));
            c.paint(col, line(x0 + t, y1, x0 + t + 2.5, y1, 1.0));
        }
        if t + 2.5 < 20.0 {
            c.paint(col, line(x0, y0 + t, x0, y0 + t + 2.5, 1.0));
            c.paint(col, line(x1, y0 + t, x1, y0 + t + 2.5, 1.0));
        }
    }
    for (x, y) in [(x0, y0), (x1, y0), (x0, y1), (x1, y1)] {
        c.paint(col, circle(x, y, 2.8));
    }
}

fn icon_screen(c: &mut Canvas, cx: f32, cy: f32, col: Rgba) {
    c.paint(col, stroke(rrect(cx - 14.0, cy - 11.0, cx + 14.0, cy + 7.0, 2.5), 2.0));
    c.paint(col, line(cx, cy + 8.0, cx, cy + 11.5, 1.0));
    c.paint(col, line(cx - 6.5, cy + 12.0, cx + 6.5, cy + 12.0, 1.1));
}

fn icon_window(c: &mut Canvas, cx: f32, cy: f32, col: Rgba, bg: Rgba) {
    c.paint(col, stroke(rrect(cx - 2.0, cy - 13.0, cx + 13.0, cy + 1.0, 2.0), 2.0));
    c.paint(col, rrect(cx - 2.0, cy - 13.0, cx + 13.0, cy - 8.5, 2.0));
    c.paint(bg, rrect(cx - 14.0, cy - 5.0, cx + 4.0, cy + 12.0, 2.0));
    c.paint(col, stroke(rrect(cx - 13.0, cy - 4.0, cx + 3.0, cy + 11.0, 2.0), 2.0));
    c.paint(col, rrect(cx - 13.0, cy - 4.0, cx + 3.0, cy + 0.5, 2.0));
}

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

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Selection,
    Screen,
    Window,
}

pub const MODES: [Mode; 3] = [Mode::Selection, Mode::Screen, Mode::Window];

#[derive(Clone, Copy, PartialEq)]
pub enum Hit {
    Close,
    Mode(Mode),
    Shot,
    Cast,
    Shutter,
    Settings,
}

// Panel geometry: GNOME 42's proportions, ~15% larger for 1080p.
const PW: f32 = 370.0;
const PH: f32 = 198.0;
const TOP: f32 = 20.0; // room above the panel for the close button
pub const PANEL_W: usize = 388; // PW plus the close button sticking out
pub const PANEL_H: usize = 218;
pub const PANEL_TOP: i32 = TOP as i32;
const MODE_X: [f32; 3] = [71.0, 185.0, 299.0];
const MODE_Y: (f32, f32) = (TOP + 18.0, TOP + 106.0);
const ROW: f32 = TOP + 153.0; // centre of the bottom row
const TOGGLE: (f32, f32) = (26.0, 124.0);
const SHUTTER_X: f32 = 185.0;
const GEAR_X: f32 = 322.0;
const CLOSE: (f32, f32, f32) = (PW - 3.0, TOP - 1.0, 19.0);

pub struct PanelState<'a> {
    pub mode: Mode,
    pub record: bool,
    /// Name of the window Window mode would record, shown under its label.
    pub window: Option<String>,
    pub hover: Option<Hit>,
    pub settings_open: bool,
    pub font: Option<&'a FontVec>,
}

pub fn panel_hit(x: i16, y: i16) -> Option<Hit> {
    let (x, y) = (x as f32, y as f32);
    let near = |cx: f32, cy: f32, r: f32| (x - cx).hypot(y - cy) <= r;
    if near(CLOSE.0, CLOSE.1, CLOSE.2) {
        return Some(Hit::Close);
    }
    if (MODE_Y.0..MODE_Y.1).contains(&y) {
        return MODE_X.iter().position(|cx| (x - cx).abs() <= 50.0).map(|i| Hit::Mode(MODES[i]));
    }
    if near(SHUTTER_X, ROW, 31.0) {
        return Some(Hit::Shutter);
    }
    let mid = (TOGGLE.0 + TOGGLE.1) / 2.0;
    match x {
        _ if (y - ROW).abs() > 19.0 => None,
        x if (TOGGLE.0..mid).contains(&x) => Some(Hit::Shot),
        x if (mid..TOGGLE.1).contains(&x) => Some(Hit::Cast),
        x if (x - GEAR_X).abs() <= 22.0 => Some(Hit::Settings),
        _ => None,
    }
}

pub fn panel(s: &PanelState) -> Canvas {
    let mut c = Canvas::new(PANEL_W, PANEL_H);
    let hov = |h| s.hover == Some(h);
    c.paint(PANEL, rrect(0.0, TOP, PW, TOP + PH, 32.0));

    for (i, &m) in MODES.iter().enumerate() {
        let cx = MODE_X[i];
        let bg = if s.mode == m { LIT } else if hov(Hit::Mode(m)) { HOVER } else { PANEL };
        c.paint(bg, rrect(cx - 50.0, MODE_Y.0, cx + 50.0, MODE_Y.1, 14.0));
        let iy = TOP + 46.0;
        match m {
            Mode::Selection => icon_selection(&mut c, cx, iy, WHITE),
            Mode::Screen => icon_screen(&mut c, cx, iy, WHITE),
            Mode::Window => icon_window(&mut c, cx, iy, WHITE, bg),
        }
        if let Some(f) = s.font {
            let label = match m {
                Mode::Selection => tr!("Selection", "Selección", "選択範囲"),
                Mode::Screen => tr!("Screen", "Pantalla", "画面"),
                Mode::Window => tr!("Window", "Ventana", "ウィンドウ"),
            };
            c.text(f, &label, 15.0, cx, TOP + 86.0, 0.5, WHITE);
            if let (Mode::Window, Some(name)) = (m, &s.window) {
                let name = Canvas::fit(f, name, 12.0, 92.0 - Canvas::width(f, "()", 12.0));
                c.text(f, &format!("({name})"), 12.0, cx, TOP + 101.0, 0.5, gray(0.7));
            }
        }
    }

    // screenshot | screencast
    let mid = (TOGGLE.0 + TOGGLE.1) / 2.0;
    c.paint(gray(0.188), rrect(TOGGLE.0, ROW - 18.0, TOGGLE.1, ROW + 18.0, 11.0));
    for (half, x0, x1) in [(Hit::Shot, TOGGLE.0 + 2.0, mid - 1.0), (Hit::Cast, mid + 1.0, TOGGLE.1 - 2.0)] {
        let on = (half == Hit::Cast) == s.record;
        let bg = if on { WHITE } else if hov(half) { gray(0.27) } else { gray(0.188) };
        c.paint(bg, rrect(x0, ROW - 16.0, x1, ROW + 16.0, 9.0));
        let (col, cx) = (if on { PANEL } else { WHITE }, (x0 + x1) / 2.0);
        if half == Hit::Shot { icon_camera(&mut c, cx, ROW, col, bg) } else { icon_video(&mut c, cx, ROW, col) }
    }

    // shutter: white for a screenshot, red to record
    c.paint(WHITE, stroke(circle(SHUTTER_X, ROW, 29.5), 3.5));
    let disc = match (s.record, hov(Hit::Shutter)) {
        (true, false) => RED,
        (true, true) => (0.75, 0.09, 0.12, 1.0),
        (false, false) => WHITE,
        (false, true) => gray(0.85),
    };
    c.paint(disc, circle(SHUTTER_X, ROW, 23.5));

    let gear_bg = if s.settings_open { LIT } else if hov(Hit::Settings) { HOVER } else { PANEL };
    c.paint(gear_bg, rrect(GEAR_X - 20.0, ROW - 18.0, GEAR_X + 20.0, ROW + 18.0, 11.0));
    icon_gear(&mut c, GEAR_X, ROW, WHITE, gear_bg);

    c.paint(if hov(Hit::Close) { gray(0.32) } else { gray(0.24) }, circle(CLOSE.0, CLOSE.1, CLOSE.2));
    let (x, y) = (CLOSE.0, CLOSE.1);
    c.paint(WHITE, line(x - 5.5, y - 5.5, x + 5.5, y + 5.5, 1.2));
    c.paint(WHITE, line(x - 5.5, y + 5.5, x + 5.5, y - 5.5, 1.2));
    c
}

#[derive(Clone, Copy, PartialEq)]
pub enum SetHit {
    Close,
    Output(usize),
    Mic,
    VideoFormat(usize),
    Gpu,
    ImageFormat(usize),
    Pointer,
    Shortcut,
    Lang(usize),
}

// Settings modal geometry.
pub const SET_W: usize = 360;
pub const SET_H: usize = 546;
const SW: f32 = SET_W as f32;
const SEG: (f32, f32) = (94.0, 130.0); // sound source segmented control, y range
const ROW_MIC: f32 = 160.0;
const ROW_VIDEO: f32 = 216.0;
const ROW_GPU: f32 = 260.0;
const ROW_SHOT: f32 = 316.0;
const ROW_POINTER: f32 = 360.0;
const ROW_KEY: f32 = 416.0;
const LANG_SEG: (f32, f32) = (494.0, 530.0); // language segmented control, y range
const PICK: (f32, f32) = (SW - 160.0, SW - 24.0); // two-option picker (MKV|MP4, PNG|JPG), x range

pub struct SettingsState<'a> {
    pub output: usize, // index into audio::OUTPUTS
    pub mic: bool,
    pub mp4: bool,
    pub gpu: bool,
    pub gpu_found: bool,
    pub jpg: bool,
    pub pointer: bool,
    pub shortcut: String,
    pub capturing: bool, // waiting for the new shortcut
    pub hover: Option<SetHit>,
    pub font: Option<&'a FontVec>,
}

pub fn settings_hit(x: i16, y: i16) -> Option<SetHit> {
    let (x, y) = (x as f32, y as f32);
    if (x - (SW - 30.0)).hypot(y - 30.0) <= 17.0 {
        return Some(SetHit::Close);
    }
    if !(24.0..SW - 24.0).contains(&x) {
        return None;
    }
    if (SEG.0..SEG.1).contains(&y) {
        return Some(SetHit::Output((((x - 24.0) / ((SW - 48.0) / 3.0)) as usize).min(2)));
    }
    if (LANG_SEG.0..LANG_SEG.1).contains(&y) {
        return Some(SetHit::Lang((((x - 24.0) / ((SW - 48.0) / 3.0)) as usize).min(2)));
    }
    let pick = (PICK.0..PICK.1).contains(&x).then(|| ((x - PICK.0) / ((PICK.1 - PICK.0) / 2.0)) as usize);
    let row = [ROW_MIC, ROW_VIDEO, ROW_GPU, ROW_SHOT, ROW_POINTER, ROW_KEY].into_iter().position(|cy| (y - cy).abs() <= 18.0)?;
    match row {
        0 => Some(SetHit::Mic),
        1 => pick.map(SetHit::VideoFormat),
        2 => Some(SetHit::Gpu),
        3 => pick.map(SetHit::ImageFormat),
        4 => Some(SetHit::Pointer),
        _ => Some(SetHit::Shortcut),
    }
}

fn switch(c: &mut Canvas, x1: f32, cy: f32, on: bool, enabled: bool) {
    let track = match (on, enabled) {
        (_, false) => gray(0.22),
        (true, true) => (0.21, 0.52, 0.89, 1.0), // GNOME blue
        (false, true) => gray(0.32),
    };
    c.paint(track, rrect(x1 - 48.0, cy - 12.0, x1, cy + 12.0, 12.0));
    c.paint(if enabled { WHITE } else { gray(0.5) }, circle(if on { x1 - 12.0 } else { x1 - 36.0 }, cy, 9.0));
}

/// Segmented control: `labels` across x0..x1 around row `cy`, `on` selected.
fn segmented(c: &mut Canvas, font: Option<&FontVec>, (x0, x1): (f32, f32), cy: f32, labels: &[&str], on: usize, hover: Option<usize>) {
    let seg_w = (x1 - x0) / labels.len() as f32;
    c.paint(gray(0.188), rrect(x0, cy - 18.0, x1, cy + 18.0, 11.0));
    for (i, label) in labels.iter().enumerate() {
        let sx = x0 + i as f32 * seg_w;
        if i == on || hover == Some(i) {
            c.paint(if i == on { WHITE } else { gray(0.27) }, rrect(sx + 2.0, cy - 16.0, sx + seg_w - 2.0, cy + 16.0, 9.0));
        }
        if let Some(f) = font {
            c.text(f, label, 15.0, sx + seg_w / 2.0, cy + 5.0, 0.5, if i == on { PANEL } else { WHITE });
        }
    }
}

pub fn settings(s: &SettingsState) -> Canvas {
    let mut c = Canvas::new(SET_W, SET_H);
    let hov = |h| s.hover == Some(h);
    let hov_i = |f: fn(usize) -> SetHit| (0..3).find(|&i| s.hover == Some(f(i)));
    c.paint(PANEL, rrect(0.0, 0.0, SW, SET_H as f32, 24.0));
    c.paint(if hov(SetHit::Close) { gray(0.32) } else { gray(0.24) }, circle(SW - 30.0, 30.0, 16.0));
    c.paint(WHITE, line(SW - 35.0, 25.0, SW - 25.0, 35.0, 1.2));
    c.paint(WHITE, line(SW - 35.0, 35.0, SW - 25.0, 25.0, 1.2));

    for (h, cy) in [SetHit::Mic, SetHit::Gpu, SetHit::Pointer].into_iter().zip([ROW_MIC, ROW_GPU, ROW_POINTER]) {
        if hov(h) {
            c.paint(HOVER, rrect(16.0, cy - 18.0, SW - 16.0, cy + 18.0, 10.0));
        }
    }
    let sound = (SEG.0 + SEG.1) / 2.0;
    segmented(&mut c, s.font, (24.0, SW - 24.0), sound, &[&tr!("None", "Ninguno", "なし"), &tr!("System", "Sistema", "システム"), &tr!("Window", "Ventana", "ウィンドウ")], s.output, hov_i(SetHit::Output));
    let lang = (LANG_SEG.0 + LANG_SEG.1) / 2.0;
    let langs = [tr!("English", "Inglés", "英語"), tr!("Spanish", "Español", "スペイン語"), tr!("Japanese", "Japonés", "日本語")];
    let on = crate::i18n::LANGS.iter().position(|&l| l == crate::i18n::lang()).unwrap();
    segmented(&mut c, s.font, (24.0, SW - 24.0), lang, &langs.each_ref().map(String::as_str), on, hov_i(SetHit::Lang));
    switch(&mut c, SW - 24.0, ROW_MIC, s.mic, true);
    segmented(&mut c, s.font, PICK, ROW_VIDEO, &["MKV", "MP4"], s.mp4 as usize, hov_i(SetHit::VideoFormat));
    switch(&mut c, SW - 24.0, ROW_GPU, s.gpu && s.gpu_found, s.gpu_found);
    segmented(&mut c, s.font, PICK, ROW_SHOT, &["PNG", "JPG"], s.jpg as usize, hov_i(SetHit::ImageFormat));
    switch(&mut c, SW - 24.0, ROW_POINTER, s.pointer, true);
    for y in [188.0, 288.0, 388.0, 444.0] {
        c.paint(gray(0.2), line(24.0, y, SW - 24.0, y, 0.5));
    }

    // shortcut: shows the current one; click, then press the new keys
    let (key_bg, key) = match (s.capturing, hov(SetHit::Shortcut)) {
        (true, _) => ((0.21, 0.52, 0.89, 1.0), tr!("Press keys…", "Pulsa las teclas…", "キーを押してください…")),
        (false, true) => (gray(0.30), s.shortcut.clone()),
        (false, false) => (gray(0.24), s.shortcut.clone()),
    };
    c.paint(key_bg, rrect(PICK.0, ROW_KEY - 16.0, PICK.1, ROW_KEY + 16.0, 9.0));

    if let Some(f) = s.font {
        c.text(f, &tr!("Settings", "Ajustes", "設定"), 19.0, 24.0, 38.0, 0.0, WHITE);
        c.text(f, &tr!("Sound", "Sonido", "サウンド"), 14.0, 24.0, 82.0, 0.0, gray(0.6));
        c.text(f, &tr!("Language", "Idioma", "言語"), 14.0, 24.0, 482.0, 0.0, gray(0.6));
        let gpu_note = if s.gpu_found { "NVENC".to_owned() } else { tr!("not found", "no encontrada", "見つかりません") };
        let use_gpu = tr!("Use GPU", "Usar GPU", "GPU を使用");
        for (label, cy) in [(tr!("Microphone", "Micrófono", "マイク"), ROW_MIC), (tr!("Video format", "Formato de video", "動画形式"), ROW_VIDEO), (use_gpu.clone(), ROW_GPU)] {
            c.text(f, &label, 15.0, 24.0, cy + 5.0, 0.0, WHITE);
        }
        c.text(f, &gpu_note, 12.0, 24.0 + Canvas::width(f, &format!("{use_gpu} "), 15.0), ROW_GPU + 5.0, 0.0, gray(0.55));
        for (label, cy) in [(tr!("Screenshot format", "Formato de captura", "スクリーンショット形式"), ROW_SHOT), (tr!("Show pointer", "Mostrar puntero", "ポインターを表示"), ROW_POINTER), (tr!("Shortcut", "Atajo", "ショートカット"), ROW_KEY)] {
            c.text(f, &label, 15.0, 24.0, cy + 5.0, 0.0, WHITE);
        }
        c.text(f, &key, 15.0, (PICK.0 + PICK.1) / 2.0, ROW_KEY + 5.0, 0.5, WHITE);
    }
    c
}

/// Recording pill button under x (1 pause/resume, 2 stop).
pub fn pill_slot(x: i16) -> usize {
    x.max(0) as usize / 40
}

/// The recording pill. Translucent on purpose: total alpha stays <= 0.8
/// (0.9 on the small dot), so capture.rs can solve the compositor's blend
/// for the pixels underneath with an error of a couple of levels.
pub fn pill(paused: bool) -> Canvas {
    let mut c = Canvas::new(120, 40);
    let icon = (1.0, 1.0, 1.0, 0.5); // 0.5 over the 0.6 backdrop -> 0.8 total
    c.paint((0.0, 0.0, 0.0, 0.6), rrect(0.0, 0.0, 120.0, 40.0, 20.0));
    let dot = if paused { (0.56, 0.56, 0.58, 0.75) } else { (1.0, 0.23, 0.19, 0.75) };
    c.paint(dot, circle(20.0, 20.0, 7.0));
    if paused {
        c.poly(icon, &[(55.0, 12.5), (67.5, 20.0), (55.0, 27.5)]);
    } else {
        c.paint(icon, line(55.0, 14.0, 55.0, 26.0, 2.5));
        c.paint(icon, line(65.0, 14.0, 65.0, 26.0, 2.5));
    }
    c.paint(icon, rrect(94.0, 14.0, 106.0, 26.0, 2.5));
    c
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

const EDGE: i32 = 20; // where the pill rests, from the screen edge

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
pub struct Pill {
    pub win: Win,
    screen: (i32, i32),
    drag: Option<Drag>,
    glide: Option<Glide>,
}

impl Pill {
    pub fn new(cap: &Capture) -> Res<Self> {
        let c = pill(false);
        let (sw, sh) = (cap.sw as i32, cap.sh as i32);
        let (x, y) = (sw - c.w as i32 - EDGE, sh - c.h as i32 - EDGE);
        let mask = EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::BUTTON1_MOTION;
        let win = Win::new(cap, x, y, c, mask)?;
        win.show(&cap.conn)?;
        Ok(Pill { win, screen: (sw, sh), drag: None, glide: None })
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
                    return Ok(match pill_slot(d.press_x) {
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
        let (x, y) = (self.win.x.clamp(EDGE, sw - w - EDGE), self.win.y.clamp(EDGE, sh - h - EDGE));
        let spots = [(cx, (EDGE, y)), (sw - cx, (sw - w - EDGE, y)), (cy, (x, EDGE)), (sh - cy, (x, sh - h - EDGE))];
        spots.into_iter().min_by_key(|s| s.0).unwrap().1
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
        self.win.redraw(conn, pill(paused))
    }
}

/// Offscreen renders of every surface, for design review:
/// `SCREENREC_PREVIEW=<dir> cargo test preview -- --ignored`.
#[cfg(test)]
mod preview {
    use super::*;
    use crate::i18n::{self, Lang};

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
        let mut c = Canvas::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let (t, u) = (x as f32 / w as f32, y as f32 / h as f32);
                let ch = |a: f32, b: f32| ((a + (b - a) * (t * 0.6 + u * 0.4)) * 255.0) as u32;
                c.px[y * w + x] = 255 << 24 | ch(0.17, 0.85) << 16 | ch(0.12, 0.42) << 8 | ch(0.33, 0.25);
            }
        }
        for (x0, y0, x1, y1, bar) in [(180.0, 120.0, 1100.0, 760.0, gray(0.92)), (820.0, 300.0, 1700.0, 900.0, gray(0.2))] {
            c.paint((0.0, 0.0, 0.0, 0.35), rrect(x0 - 6.0, y0 - 2.0, x1 + 6.0, y1 + 12.0, 16.0));
            c.paint(if bar.0 > 0.5 { WHITE } else { gray(0.14) }, rrect(x0, y0, x1, y1, 10.0));
            c.paint(bar, rrect(x0, y0, x1, y0 + 46.0, 10.0));
            for i in 0..8 {
                let y = y0 + 90.0 + i as f32 * 48.0;
                let col = if bar.0 > 0.5 { gray(0.75) } else { gray(0.35) };
                c.paint(col, rrect(x0 + 40.0, y, x0 + 40.0 + (x1 - x0 - 80.0) * (0.4 + 0.07 * (i % 5) as f32), y + 14.0, 7.0));
            }
        }
        c
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
        let (latin, ja) = (load_font(false), load_font(true));
        for (lang, tag) in [(Lang::En, "en"), (Lang::Es, "es"), (Lang::Ja, "ja")] {
            i18n::set(lang);
            let font = if lang == Lang::Ja { ja.as_ref() } else { latin.as_ref() };
            let panels = [
                ("shot", PanelState { mode: Mode::Selection, record: false, window: None, hover: None, settings_open: false, font }),
                ("rec-hover", PanelState { mode: Mode::Window, record: true, window: Some("Firefox Web Browser".into()), hover: Some(Hit::Shutter), settings_open: false, font }),
                ("hover-gear", PanelState { mode: Mode::Screen, record: false, window: None, hover: Some(Hit::Mode(Mode::Window)), settings_open: true, font }),
            ];
            for (name, s) in &panels {
                let c = panel(s);
                save(&dir, &format!("panel-{name}-{tag}"), c.w, c.h, &c.px, bg);
            }
            let set = |hover, capturing, gpu_found| SettingsState {
                output: 1,
                mic: true,
                mp4: false,
                gpu: true,
                gpu_found,
                jpg: false,
                pointer: false,
                shortcut: "Ctrl+Shift+S".into(),
                capturing,
                hover,
                font,
            };
            for (name, s) in [("default", set(None, false, true)), ("hover-capturing", set(Some(SetHit::Pointer), true, false))] {
                let c = settings(&s);
                save(&dir, &format!("settings-{name}-{tag}"), c.w, c.h, &c.px, bg);
            }
            // The whole launcher over the frozen desktop, as the user sees it.
            let sel = (420, 260, 1240, 720);
            let mut full: Vec<u32> = crate::select::preview(frozen.clone(), sw, sh, Some(sel), true).chunks_exact(4).map(|p| 255 << 24 | u32::from_le_bytes([p[0], p[1], p[2], 0])).collect();
            let (px, py) = ((sw - PANEL_W) / 2, sh - PANEL_H - 48);
            over(&mut full, sw, &panel(&panels[0].1), px, py);
            save(&dir, &format!("launcher-{tag}"), sw, sh, &full, bg);
            let (mx, my) = ((sw - SET_W) / 2, py + PANEL_TOP as usize - SET_H - 14);
            over(&mut full, sw, &settings(&set(None, false, true)), mx, my);
            save(&dir, &format!("launcher-settings-{tag}"), sw, sh, &full, bg);
        }
        // The pill over light and dark backgrounds.
        for (name, paused) in [("recording", false), ("paused", true)] {
            let c = pill(paused);
            save(&dir, &format!("pill-{name}-light"), c.w, c.h, &c.px, [0xf2, 0xf2, 0xf2]);
            save(&dir, &format!("pill-{name}-dark"), c.w, c.h, &c.px, [0x24, 0x24, 0x24]);
        }
    }
}
