//! screenrec: fast X11 screenshots and screen recording (H.264 on the GPU with
//! NVENC, or on the CPU with x264; MKV or MP4), from the command line or a
//! launcher modelled on GNOME 42's screenshot UI.

#[macro_use]
mod i18n;
mod audio;
#[cfg_attr(windows, path = "windows/capture.rs")]
#[cfg_attr(target_os = "macos", path = "macos/capture.rs")]
mod capture;
#[cfg_attr(windows, path = "windows/desktop.rs")]
#[cfg_attr(target_os = "macos", path = "macos/desktop.rs")]
mod desktop;
mod dylib;
mod frame;
mod mkv;
mod nvenc;
#[allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code, unused_imports, clippy::all)]
mod nvenc_sys;
#[cfg(target_os = "linux")]
mod select;
mod service;
#[cfg(target_os = "linux")]
mod shortcut;
#[cfg(target_os = "linux")]
mod ui;
mod x264;

use audio::Output;
use capture::Capture;
use desktop::notify;
use frame::{Rect, Sprite, View};
#[cfg(target_os = "linux")]
use std::io::Write;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
#[cfg(target_os = "linux")]
use ui::{Hit, Mode, Pill, PillEvent, SetHit};
#[cfg(target_os = "linux")]
use x11rb::CURRENT_TIME;
#[cfg(target_os = "linux")]
use x11rb::connection::Connection;
#[cfg(target_os = "linux")]
use x11rb::protocol::Event;
#[cfg(target_os = "linux")]
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, EventMask, GrabMode, GrabStatus};

pub type Res<T> = Result<T, Box<dyn std::error::Error>>;

fn usage() -> String {
    tr!(
        "usage:
  screenrec                             launcher: screenshot or recording (selection, screen or window)
  screenrec shot [file.png|.jpg]        full-screen screenshot
  screenrec rec [file.mkv|.mp4] [-r FPS] [--window ID] [--cpu]
                                        record the screen (or a window) until Ctrl+C / SIGTERM
                                        (max FPS: 60 on the GPU, 30 without it; --cpu: no GPU even if there is one)
  screenrec install                     keyboard shortcut for the launcher ('-' if it has none yet)",
        "uso:
  screenrec                             interfaz: captura o grabación (selección, pantalla o ventana)
  screenrec shot [archivo.png|.jpg]     captura de pantalla completa
  screenrec rec [archivo.mkv|.mp4] [-r FPS] [--window ID] [--cpu]
                                        graba la pantalla (o una ventana) hasta Ctrl+C / SIGTERM
                                        (máx. FPS: 60 con GPU, 30 sin ella; --cpu: sin GPU aunque haya)
  screenrec install                     atajo de teclado para la interfaz ('-' si aún no tiene)",
        "使い方:
  screenrec                             ランチャー: スクリーンショットまたは録画 (選択範囲、画面、ウィンドウ)
  screenrec shot [ファイル.png|.jpg]    画面全体のスクリーンショット
  screenrec rec [ファイル.mkv|.mp4] [-r FPS] [--window ID] [--cpu]
                                        Ctrl+C / SIGTERM まで画面 (またはウィンドウ) を録画
                                        (最大 FPS: GPU で 60、なしで 30。--cpu: GPU があっても使わない)
  screenrec install                     ランチャーのキーボードショートカット (未設定なら '-')"
    )
}

/// Wall time between forced keyframes (seek granularity).
const KEYINT_MS: u64 = 5000;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Relaxed);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(l) = saved_lang() {
        i18n::set(l);
    }
    let res = match args.first().map(String::as_str) {
        None => gui(),
        Some("shot") => shot(args.get(1)),
        Some("rec") => rec(&args[1..]),
        Some("install") => install(),
        #[cfg(target_os = "linux")]
        Some(desktop::CLIP_OWNER) => desktop::own_clipboard(args.get(1)),
        _ => {
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    };
    if let Err(e) = res {
        eprintln!("error: {e}");
        if args.is_empty() {
            notify(&tr!("screenrec ran into a problem", "screenrec tuvo un problema", "screenrec で問題が発生しました"), &e.to_string(), None); // the GUI has no terminal
        }
        std::process::exit(1);
    }
}

/// Point our GNOME shortcut at this executable; '-' unless one was already picked.
#[cfg(target_os = "linux")]
fn install() -> Res<()> {
    let accel = shortcut::get().unwrap_or_else(|| "minus".into());
    shortcut::set(&accel)?;
    println!("{}", tr!("launcher shortcut: {}", "atajo de la interfaz: {}", "ランチャーのショートカット: {}", shortcut::pretty(&accel)));
    Ok(())
}

fn shot(out: Option<&String>) -> Res<()> {
    let mut cap = Capture::new()?;
    freeze(&mut cap)?;
    let path = out.map(PathBuf::from).unwrap_or_else(|| default_path("PICTURES", &shot_prefix(), "png"));
    save_image(cap.frame(), cap.sw, (0, 0, cap.sw as i32, cap.sh as i32), None, &path)?;
    println!("{}", path.display());
    Ok(())
}

fn rec(args: &[String]) -> Res<()> {
    let (mut opts, mut path, mut window) = (RecOpts { fps: None, gpu: true, sound: None }, None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-r" => opts.fps = Some(it.next().and_then(|v| v.parse().ok()).filter(|f| (1..=240).contains(f)).ok_or(tr!("-r expects 1..240", "-r espera 1..240", "-r には 1..240 を指定してください"))?),
            "--cpu" => opts.gpu = false,
            "--window" => {
                let id = it.next().ok_or(tr!("--window expects the window id", "--window espera el id de la ventana", "--window にはウィンドウ ID を指定してください"))?;
                let id = id.strip_prefix("0x").map_or_else(|| id.parse().ok(), |h| u32::from_str_radix(h, 16).ok());
                window = Some(id.ok_or(tr!("invalid window id", "id de ventana inválido", "無効なウィンドウ ID です"))?);
            }
            p => path = Some(PathBuf::from(p)),
        }
    }
    let path = path.unwrap_or_else(|| default_path("VIDEOS", &rec_prefix(), "mkv"));
    let mp4 = match path.extension().and_then(|e| e.to_str()) {
        Some("mp4") => true,
        Some("mkv") => false,
        _ => return Err(tr!("recordings are .mkv or .mp4", "se graba en .mkv o .mp4", "録画は .mkv または .mp4 のみです").into()),
    };
    let rec_path = if mp4 { path.with_extension("rec.mkv") } else { path.clone() }; // MP4 comes out of the MKV at the end
    let mut cap = Capture::new()?;
    let target = match window {
        Some(w) => Target::Window(w, cap.window_area(w)?),
        None => Target::Area((0, 0, cap.sw as i32, cap.sh as i32)),
    };
    record(&mut cap, &rec_path, &opts, None, None, target)?;
    if mp4 {
        to_mp4(&rec_path, &path)?;
    }
    Ok(())
}

/// How to record: frame rate cap (default: 60 on the GPU, 30 on the CPU,
/// where encoding is what costs), GPU or not, and the sound (output,
/// microphone, the pid whose sound "Window" means).
struct RecOpts {
    fps: Option<u32>,
    gpu: bool,
    sound: Option<(Output, bool, Option<u32>)>,
}

/// What a recording captures: part of the screen, or one window (client id,
/// where it shows), which keeps recording while covered or moved.
#[derive(Clone, Copy)]
enum Target {
    Area(Rect),
    Window(u32, Rect),
}

/// The whole screen as it is now, without the pointer, left in `cap.frame()`;
/// the pointer is returned apart, to be drawn back in on request. X11 has no
/// "leave the cursor out" here, so hide it and grab until it is verifiably gone.
fn freeze(cap: &mut Capture) -> Res<Sprite> {
    cap.screen_readable()?;
    let (cursor, _) = cap.query_cursor()?;
    let hidden = cap.hide_pointer()?;
    let give_up = Instant::now() + Duration::from_millis(500);
    loop {
        cap.grab((0, cap.sh as i32))?;
        if !(hidden && cap.shows(&cursor)) || Instant::now() > give_up {
            break;
        }
        std::thread::sleep(Duration::from_millis(4));
    }
    cap.show_pointer()?;
    Ok(cursor)
}

/// Cut `r` out of the BGRX `screen` and save it as PNG, or JPG if the path says so.
fn save_image(screen: &[u8], sw: usize, r: Rect, cursor: Option<&Sprite>, path: &Path) -> Res<()> {
    let (w, h) = ((r.2 - r.0) as usize, (r.3 - r.1) as usize);
    let rows = || (r.1..r.3).map(|y| &screen[(y as usize * sw + r.0 as usize) * 4..][..w * 4]);
    // Full-width rows are already one contiguous image; a cut-out or a
    // drawn-in pointer needs its own copy.
    let mut own = Vec::new();
    let img: &[u8] = if cursor.is_none() && w == sw {
        &screen[r.1 as usize * sw * 4..r.3 as usize * sw * 4]
    } else {
        own.reserve_exact(w * h * 4);
        rows().for_each(|row| own.extend_from_slice(row));
        if let Some(c) = cursor {
            frame::draw(&mut own, View { w, h, x0: r.0, y0: r.1 }, c);
        }
        &own
    };
    if path.extension().is_some_and(|e| e == "jpg" || e == "jpeg") {
        let jpg = jpeg_encoder::Encoder::new_file(path, 90)?;
        jpg.encode(img, w as u16, h as u16, jpeg_encoder::ColorType::Bgra)?; // the 4th byte is ignored
        return Ok(());
    }
    let mut rgb = vec![0u8; w * h * 3];
    for (o, p) in rgb.as_chunks_mut::<3>().0.iter_mut().zip(img.as_chunks::<4>().0) {
        *o = [p[2], p[1], p[0]];
    }
    let mut png = png::Encoder::new(std::fs::File::create(path)?, w as u32, h as u32);
    png.set_color(png::ColorType::Rgb);
    png.set_compression(png::Compression::Fast);
    let mut wr = png.write_header()?;
    wr.write_image_data(&rgb)?;
    wr.finish()?;
    Ok(())
}

/// The language picked in the launcher's settings (the last file's 13th field).
fn saved_lang() -> Option<i18n::Lang> {
    let text = std::fs::read_to_string(last_path()).ok()?;
    let name = text.split_whitespace().nth(12)?;
    i18n::LANGS.into_iter().find(|l| format!("{l:?}") == name)
}

/// What the launcher remembers between runs.
#[cfg(target_os = "linux")]
struct Last {
    mode: Mode,
    record: bool,
    pointer: bool,
    sel: Rect,
    output: Output,
    mic: bool,
    mp4: bool,
    jpg: bool,
    gpu: bool,
}

fn last_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    let config = std::env::var("XDG_CONFIG_HOME").unwrap_or(home + "/.config");
    Path::new(&config).join("screenrec/last")
}

#[cfg(target_os = "linux")]
impl Last {
    fn load(sw: i32, sh: i32) -> Self {
        let text = std::fs::read_to_string(last_path()).unwrap_or_default();
        let f: Vec<&str> = text.split_whitespace().collect();
        let n = |i: usize| f.get(i).and_then(|v| v.parse().ok());
        let named = |i: usize, name: String| f.get(i) == Some(&name.as_str());
        let mode = ui::MODES.into_iter().find(|m| named(0, format!("{m:?}")));
        let sel = match (n(3), n(4), n(5), n(6)) {
            (Some(x0), Some(y0), Some(x1), Some(y1)) if x0 < x1 && y0 < y1 && x1 <= sw && y1 <= sh => (x0, y0, x1, y1),
            _ => (sw / 4, sh / 4, sw * 3 / 4, sh * 3 / 4),
        };
        let output = audio::OUTPUTS.into_iter().find(|o| named(7, format!("{o:?}"))).unwrap_or(Output::None);
        let flag = |i: usize| f.get(i) == Some(&"true");
        let gpu = f.get(11) != Some(&"false"); // the GPU when there is one, unless told otherwise
        let mode = mode.unwrap_or(Mode::Selection);
        Last { mode, record: flag(1), pointer: flag(2), sel, output, mic: flag(8), mp4: flag(9), jpg: flag(10), gpu }
    }

    fn save(&self) {
        let path = last_path();
        let (m, (x0, y0, x1, y1), o) = (self.mode, self.sel, self.output);
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let (rec, ptr, mic, mp4, jpg, gpu) = (self.record, self.pointer, self.mic, self.mp4, self.jpg, self.gpu);
        let lang = i18n::chosen().map_or("-".into(), |l| format!("{l:?}")); // "-": the locale's
        let _ = std::fs::write(path, format!("{m:?} {rec} {ptr} {x0} {y0} {x1} {y1} {o:?} {mic} {mp4} {jpg} {gpu} {lang}\n"));
    }
}

/// One launcher at a time: launching again (the shortcut pressed twice)
/// closes the first one, or stops its recording, and exits.
#[cfg(target_os = "linux")]
fn single_instance() -> Res<Option<std::fs::File>> {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let path = Path::new(&dir).join("screenrec.lock");
    let mut f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        if let Ok(pid) = std::fs::read_to_string(&path)?.trim().parse::<i32>() {
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        return Ok(None);
    }
    f.set_len(0)?;
    write!(f, "{}", std::process::id())?;
    Ok(Some(f))
}

/// Keycode -> unshifted keysym.
#[cfg(target_os = "linux")]
struct Keymap {
    min: u8,
    per: usize,
    syms: Vec<u32>,
}

#[cfg(target_os = "linux")]
impl Keymap {
    fn new(cap: &Capture) -> Res<Self> {
        let (min, max) = (cap.conn.setup().min_keycode, cap.conn.setup().max_keycode);
        let map = cap.conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
        Ok(Keymap { min, per: map.keysyms_per_keycode as usize, syms: map.keysyms })
    }

    fn sym(&self, code: u8) -> u32 {
        self.syms.get((code.saturating_sub(self.min)) as usize * self.per).copied().unwrap_or(0)
    }
}

#[cfg(target_os = "linux")]
const KEY_ESCAPE: u32 = 0xff1b;
#[cfg(target_os = "linux")]
const KEY_TAB: u32 = 0xff09;
#[cfg(target_os = "linux")]
const KEYS_PREV: [u32; 5] = [0xfe20, 0xff51, 0xff52, 0xff96, 0xff97]; // ISO_Left_Tab, Left, Up, KP_Left, KP_Up
#[cfg(target_os = "linux")]
const KEYS_NEXT: [u32; 4] = [0xff53, 0xff54, 0xff98, 0xff99]; // Right, Down, KP_Right, KP_Down
#[cfg(target_os = "linux")]
const KEY_ENTERS: [u32; 3] = [0xff0d, 0xff8d, 0x20]; // Return, KP_Enter, space

/// A control being activated: a click and Enter on the focus ring do the same.
#[cfg(target_os = "linux")]
enum Press {
    Panel(Hit),
    Settings(SetHit),
}

/// The desktop's UI scale: Xft.dpi / 96 (GNOME's text scaling sets it), else 1.
#[cfg(target_os = "linux")]
fn ui_scale(cap: &Capture) -> f32 {
    let db = cap.conn.get_property(false, cap.root, AtomEnum::RESOURCE_MANAGER, AtomEnum::STRING, 0, 1 << 16).ok().and_then(|r| r.reply().ok());
    let dpi = db.and_then(|p| String::from_utf8_lossy(&p.value).lines().find_map(|l| l.strip_prefix("Xft.dpi:")?.trim().parse::<f32>().ok()));
    dpi.map_or(1.0, |d| (d / 96.0).clamp(1.0, 3.0))
}

/// Grab the keyboard so the launcher gets its keys. Best effort: the
/// shortcut that launched us may hold a grab for a moment.
#[cfg(target_os = "linux")]
fn grab_keyboard(cap: &Capture, win: u32) -> Res<()> {
    for _ in 0..20 {
        if cap.conn.grab_keyboard(false, win, CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)?.reply()?.status == GrabStatus::SUCCESS {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// The launcher, like GNOME's: the screen freezes, pick Selection / Screen /
/// Window and screenshot or screencast, then the shutter (or Enter). The gear
/// opens the settings: sound, pointer, shortcut.
#[cfg(target_os = "linux")]
fn gui() -> Res<()> {
    let Some(_lock) = single_instance()? else { return Ok(()) };
    std::thread::spawn(|| ui::set_reduced_motion(shortcut::animations_off())); // a gsettings call: not on the way to the first frame
    let fonts_job = std::thread::spawn(|| (ui::load_font(false), ui::load_bold())); // fc-match runs while the screen is set up
    let mut cap = Capture::new()?;
    let cursor = freeze(&mut cap)?; // the buffer keeps that screen until a recording starts: overlay and screenshots read it
    let (sw, sh) = (cap.sw as i32, cap.sh as i32);
    let ewmh = select::Ewmh::new(&cap);
    let wins = ewmh.windows(&cap);
    let window_at = |x: i32, y: i32| wins.iter().copied().find(|&(r, _)| select::contains(r, x, y));
    let mut last = Last::load(sw, sh);
    let ((latin, latin_bold), ja) = (fonts_job.join().unwrap_or((None, None)), std::cell::OnceCell::new());
    let font = || match i18n::lang() {
        i18n::Lang::Ja => ja.get_or_init(|| ui::load_font(true)).as_ref(),
        _ => latin.as_ref(),
    };
    let fonts = || (font(), if i18n::lang() == i18n::Lang::Ja { font() } else { latin_bold.as_ref().or(font()) }); // regular, medium
    let pointer = cap.conn.query_pointer(cap.root)?.reply()?;
    let (mut hovered, mut picked) = (window_at(pointer.root_x as i32, pointer.root_y as i32), None);
    let area = |last: &Last, w: Option<(Rect, u32)>| match last.mode {
        Mode::Selection => Some(last.sel),
        Mode::Screen => Some((0, 0, sw, sh)),
        Mode::Window => w.map(|(r, _)| r),
    };

    let scale = ui_scale(&cap);
    let mut ov = select::Overlay::new(&cap, area(&last, hovered), last.mode == Mode::Selection)?;
    ov.scale = scale;
    let name_of = |cap: &Capture, w: Option<(Rect, u32)>| w.and_then(|(_, id)| ewmh.name(cap, id));
    let cjk = || match i18n::lang() {
        i18n::Lang::Ja => None,
        _ => ja.get_or_init(|| ui::load_font(true)).as_ref(), // loaded when the settings first open
    };
    let mut st = ui::PanelState::new(last.mode, last.record, fonts(), scale);
    let mut window = name_of(&cap, hovered); // the name of the window Window mode would take, for the badge
    st.reveal();
    let ((px, py), (mx, my)) = ui::place(sw, sh, scale);
    let mask = EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION | EventMask::LEAVE_WINDOW;
    // The size badge sits under the panel in the stack (made first): the selection's size,
    // or the window's name and size; Screen mode has none.
    let gap = (14.0 * scale).round() as i32; // clear of the corner brackets
    let badge_for = |(r, name): &(Rect, Option<String>)| {
        let c = ui::badge(r.2 - r.0, r.3 - r.1, name.as_deref(), font(), scale);
        (ui::badge_pos(*r, (c.w as i32, c.h as i32), (sw, sh), gap, ui::panel_rect(sw, sh, scale)), c)
    };
    let badge_want = |last: &Last, w: Option<(Rect, u32)>, name: &Option<String>| match last.mode {
        Mode::Selection => Some((last.sel, None)),
        Mode::Window => w.map(|(r, _)| (r, name.clone())),
        Mode::Screen => None,
    };
    let mut badge_at = badge_want(&last, hovered, &window); // what it shows, if mapped
    let ((bx, by), bc) = badge_for(&badge_at.clone().unwrap_or((last.sel, None)));
    let mut badge = ui::Win::new(&cap, bx, by, bc, EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION)?;
    let mut panel = ui::Win::new(&cap, px, py, ui::panel(&st), mask)?;
    let shortcut_now = || shortcut::get().map_or(tr!("none", "ninguno", "なし"), |a| shortcut::pretty(&a));
    let mut set = ui::SettingsState::new(fonts(), None, scale);
    // The shortcut (two gsettings runs), the NVIDIA probe (a driver dlopen) and the
    // render wait for the gear: none of them is needed to show the panel.
    (set.output, set.mic, set.pointer) = (audio::OUTPUTS.iter().position(|&o| o == last.output).unwrap_or(0), last.mic, last.pointer);
    (set.mp4, set.jpg, set.gpu) = (last.mp4, last.jpg, last.gpu);
    let mut modal = ui::Win::new(&cap, mx, my, ui::Canvas::new(ui::SW, ui::SH, scale), mask)?; // drawn and mapped by the gear
    ov.show(&cap.conn, cap.frame())?;
    if badge_at.is_some() {
        badge.show(&cap.conn)?;
    }
    panel.show(&cap.conn)?;
    let keys = Keymap::new(&cap)?;
    grab_keyboard(&cap, ov.win)?;

    let mut grip: Option<(select::Grip, Rect)> = None; // dragging, and the selection before it
    let (mut moving, mut moving_set) = (true, false); // animating: the next frame is due
    let (mut pfocus, mut sfocus, mut ring) = (Hit::Shutter, ui::SETTINGS_ORDER[0], false); // keyboard focus; the ring shows once a key moves it
    let (pid, mid, bid) = (panel.id, modal.id, badge.id);
    let mut thru = false; // a press began in a transparent margin: the drag belongs to the overlay
    loop {
        cap.wait((moving || moving_set).then_some(Duration::from_millis(8)))?; // the next animation frame, else sleep until an event
        let (mut redraw, mut reshape, mut restyle) = (false, false, false); // panel, overlay, settings
        macro_rules! close_settings {
            () => {
                (st.settings_open, set.capturing, redraw, pfocus) = (false, false, true, Hit::Settings);
                cap.conn.unmap_window(mid)?;
            };
        }
        for mut ev in cap.take_events()? {
            // The panels' transparent margins and the badge are click-through: the overlay gets those
            // events, or the panel where the settings' margin overhangs it (its close button).
            let body = |id: u32, x: i16, y: i16| id != bid && (id != pid || ui::panel_body_has(scale, x, y)) && (id != mid || ui::settings_body_has(scale, x, y));
            let under = |id: u32, rx: i16, ry: i16| {
                let (x, y) = ((rx as i32 - px) as i16, (ry as i32 - py) as i16);
                if id == mid && ui::panel_body_has(scale, x, y) { (pid, x, y) } else { (ov.win, rx, ry) }
            };
            match &mut ev {
                Event::ButtonPress(e) => {
                    ring = false;
                    if !body(e.event, e.event_x, e.event_y) {
                        let to = under(e.event, e.root_x, e.root_y);
                        (thru, e.event, e.event_x, e.event_y) = (to.0 == ov.win, to.0, to.1, to.2);
                    }
                }
                Event::ButtonRelease(e) if thru || !body(e.event, e.event_x, e.event_y) => {
                    (thru, e.event, e.event_x, e.event_y) = (false, ov.win, e.root_x, e.root_y);
                }
                Event::MotionNotify(e) if thru || !body(e.event, e.event_x, e.event_y) => {
                    let to = if thru { (ov.win, e.root_x, e.root_y) } else { under(e.event, e.root_x, e.root_y) };
                    let off = to.0 == ov.win; // on neither card
                    (redraw, st.hover) = (redraw || (off && st.hover.is_some()), if off { None } else { st.hover });
                    (restyle, set.hover) = (restyle || set.hover.is_some(), None);
                    (e.event, e.event_x, e.event_y) = to;
                }
                _ => {}
            }
            let mut press = None; // a control to activate, and whether by key
            let mut shoot = match ev {
                Event::Expose(e) if e.window == pid || e.window == mid || e.window == bid => {
                    [&panel, &modal, &badge].into_iter().find(|w| w.id == e.window).unwrap().draw(&cap.conn)?;
                    false
                }
                Event::MotionNotify(e) if e.event == panel.id => {
                    let h = ui::panel_hit(scale, e.event_x, e.event_y);
                    (redraw, st.hover) = (redraw || h != st.hover, h);
                    false
                }
                Event::MotionNotify(e) if e.event == modal.id => {
                    let h = ui::settings_hit(scale, e.event_x, e.event_y);
                    (restyle, set.hover) = (restyle || h != set.hover, h);
                    false
                }
                Event::LeaveNotify(e) if e.event == panel.id => {
                    (redraw, st.hover) = (redraw || st.hover.is_some(), None);
                    false
                }
                Event::LeaveNotify(e) if e.event == modal.id => {
                    (restyle, set.hover) = (restyle || set.hover.is_some(), None);
                    false
                }
                Event::ButtonPress(e) if e.event == pid && e.detail == 1 => {
                    press = ui::panel_hit(scale, e.event_x, e.event_y).map(|h| (Press::Panel(h), false));
                    false
                }
                Event::ButtonPress(e) if e.event == mid && e.detail == 1 => {
                    press = ui::settings_hit(scale, e.event_x, e.event_y).map(|h| (Press::Settings(h), false));
                    false
                }
                Event::ButtonPress(e) if e.event == ov.win && e.detail == 1 && st.settings_open => {
                    // A click outside closes the settings, like a popover.
                    close_settings!();
                    false
                }
                Event::ButtonPress(e) if e.event == ov.win && e.detail == 1 => {
                    let (x, y) = (e.event_x as i32, e.event_y as i32);
                    match st.mode {
                        Mode::Selection => grip = Some((select::grip(last.sel, x, y), last.sel)),
                        Mode::Window => {
                            (picked, reshape) = (window_at(x, y), true);
                            window = name_of(&cap, picked);
                        }
                        Mode::Screen => {}
                    }
                    false
                }
                Event::MotionNotify(e) if e.event == ov.win => {
                    let (x, y) = (e.event_x as i32, e.event_y as i32);
                    match (st.mode, grip) {
                        (Mode::Selection, Some((g, _))) => (last.sel, reshape) = (select::drag(g, x, y, (sw, sh)), true),
                        (Mode::Selection, None) => ov.set_cursor(&cap.conn, select::grip_cursor(last.sel, x, y))?,
                        (Mode::Window, _) if picked.is_none() && window_at(x, y) != hovered => {
                            (hovered, reshape) = (window_at(x, y), true);
                            window = name_of(&cap, hovered);
                        }
                        _ => {}
                    }
                    false
                }
                Event::ButtonRelease(e) if e.event == ov.win && e.detail == 1 => {
                    if let Some((_, before)) = grip.take()
                        && (last.sel.2 - last.sel.0 < 4 || last.sel.3 - last.sel.1 < 4)
                    {
                        (last.sel, reshape) = (before, true); // a click, not a selection
                    }
                    false
                }
                Event::KeyPress(e) => {
                    let sym = keys.sym(e.detail);
                    if set.capturing {
                        if sym == KEY_ESCAPE {
                            (set.capturing, restyle) = (false, true);
                        } else if let Some(a) = shortcut::accel(sym, e.state.into()) {
                            if let Err(err) = shortcut::set(&a) {
                                notify(&tr!("Couldn't change the shortcut", "No se pudo cambiar el atajo", "ショートカットを変更できませんでした"), &err.to_string(), None);
                            }
                            (set.shortcut, set.capturing, restyle) = (shortcut_now(), false, true);
                        }
                        false
                    } else if sym == KEY_ESCAPE && st.settings_open {
                        close_settings!();
                        false
                    } else if sym == KEY_ESCAPE {
                        return Ok(());
                    } else if sym == KEY_TAB || KEYS_PREV.contains(&sym) || KEYS_NEXT.contains(&sym) {
                        let back = if sym == KEY_TAB { u16::from(e.state) & 1 != 0 } else { KEYS_PREV.contains(&sym) }; // Shift is bit 1
                        if ring && st.settings_open {
                            sfocus = ui::step(&ui::SETTINGS_ORDER, sfocus, back, |h| h == SetHit::Gpu && !set.gpu_found);
                        } else if ring {
                            pfocus = ui::step(&ui::PANEL_ORDER, pfocus, back, |_| false);
                        }
                        ring = true; // the first press only shows where the focus is
                        false
                    } else if !KEY_ENTERS.contains(&sym) {
                        false
                    } else if ring {
                        press = Some((if st.settings_open { Press::Settings(sfocus) } else { Press::Panel(pfocus) }, true));
                        false
                    } else {
                        true
                    }
                }
                _ => false,
            };
            match press {
                Some((Press::Panel(h), key)) => {
                    redraw = true;
                    match h {
                        Hit::Close => return Ok(()),
                        Hit::Mode(m) => (st.mode, reshape) = (m, true),
                        Hit::Shot => st.record = false,
                        Hit::Cast => st.record = true,
                        Hit::Settings if st.settings_open => {
                            close_settings!();
                        }
                        Hit::Settings => {
                            (st.settings_open, set.shortcut, set.capturing, set.cjk, sfocus) = (true, shortcut_now(), false, cjk(), ui::SETTINGS_ORDER[0]);
                            set.gpu_found = nvenc::available();
                            (set.hover, set.focus) = (None, ring.then_some(sfocus));
                            set.settle();
                            set.reveal();
                            modal.redraw(&cap.conn, ui::settings(&set))?;
                            cap.conn.map_window(mid)?;
                        }
                        Hit::Shutter => shoot = true,
                    }
                    if key && matches!(h, Hit::Mode(_) | Hit::Shot | Hit::Cast) {
                        pfocus = Hit::Shutter; // pick, then Enter again captures
                    }
                }
                Some((Press::Settings(h), _)) => {
                    restyle = true;
                    match h {
                        SetHit::Close => {
                            close_settings!();
                        }
                        SetHit::Output(i) => set.output = i,
                        SetHit::Mic => set.mic = !set.mic,
                        SetHit::Pointer => set.pointer = !set.pointer,
                        SetHit::VideoFormat(i) => set.mp4 = i == 1,
                        SetHit::Gpu => set.gpu = !set.gpu,
                        SetHit::ImageFormat(i) => set.jpg = i == 1,
                        SetHit::Shortcut => set.capturing = true,
                        SetHit::Lang(i) => {
                            i18n::set(i18n::LANGS[i]);
                            ((st.font, st.bold), (set.font, set.bold)) = (fonts(), fonts());
                            (set.cjk, set.shortcut, redraw, reshape, badge_at) = (cjk(), shortcut_now(), true, true, None); // the badge, in the new font
                        }
                    }
                    // Settings stick at once, also when the launcher is then closed.
                    (last.pointer, last.output, last.mic) = (set.pointer, audio::OUTPUTS[set.output], set.mic);
                    (last.mp4, last.jpg, last.gpu) = (set.mp4, set.jpg, set.gpu);
                    last.save();
                }
                None => {}
            }
            if shoot {
                (last.mode, last.record) = (st.mode, st.record);
                let target = match (last.mode, picked.or(hovered)) {
                    (Mode::Window, Some((r, id))) => Some(Target::Window(id, r)),
                    (Mode::Window, None) => None, // no window picked
                    _ => area(&last, None).map(Target::Area),
                };
                // Whose sound "Window" records: that window, else the one in the middle of the area.
                let app = match target {
                    Some(Target::Window(id, _)) => Some(id),
                    Some(Target::Area(r)) => window_at((r.0 + r.2) / 2, (r.1 + r.3) / 2).map(|(_, id)| id),
                    None => None,
                };
                let app = app.and_then(|id| ewmh.pid(&cap, id));
                return shutter(cap, &mut ov, &[&panel, &modal, &badge], target, &last, &cursor, app, (fonts(), scale));
            }
        }
        if reshape {
            last.mode = st.mode;
            if st.mode != Mode::Selection {
                ov.set_cursor(&cap.conn, select::CURSOR_ARROW)?;
            }
            ov.set(&cap.conn, cap.frame(), area(&last, picked.or(hovered)), st.mode == Mode::Selection)?;
            let want = badge_want(&last, picked.or(hovered), &window);
            if want != badge_at {
                match &want {
                    Some(b) => {
                        let ((x, y), c) = badge_for(b);
                        badge.reset(&cap.conn, x, y, c)?;
                        cap.conn.map_window(bid)?;
                    }
                    None => {
                        cap.conn.unmap_window(bid)?;
                    }
                }
                badge_at = want;
            }
        }
        let f = (ring && !st.settings_open).then_some(pfocus);
        (redraw, st.focus) = (redraw || f != st.focus, f);
        let f = ring.then_some(sfocus);
        (restyle, set.focus) = (restyle || f != set.focus, f);
        st.sync();
        if redraw || moving || st.busy() {
            panel.redraw(&cap.conn, ui::panel(&st))?;
        }
        moving = st.busy();
        if st.settings_open {
            set.sync();
            if restyle || moving_set || set.busy() {
                modal.redraw(&cap.conn, ui::settings(&set))?;
            }
        }
        moving_set = st.settings_open && set.busy();
    }
}

/// Screenshot: cut the target out of the frozen screen. Screencast: take the
/// UI down and record the target live, with the sound picked in the settings
/// (`app`: the process whose sound "Window" means).
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn shutter(mut cap: Capture, ov: &mut select::Overlay, windows: &[&ui::Win], target: Option<Target>, last: &Last, cursor: &Sprite, app: Option<u32>, (fonts, scale): ((Option<&ab_glyph::FontVec>, Option<&ab_glyph::FontVec>), f32)) -> Res<()> {
    let Some(target) = target else { return Ok(()) }; // Window mode with no window picked
    let (Target::Area(r) | Target::Window(_, r)) = target;
    last.save();
    if !last.record {
        let path = default_path("PICTURES", &shot_prefix(), if last.jpg { "jpg" } else { "png" });
        save_image(cap.frame(), cap.sw, r, last.pointer.then_some(cursor), &path)?;
        notify(&tr!("Screenshot saved", "Captura guardada", "スクリーンショットを保存しました"), &tilde(&path), Some(&path));
        return Ok(());
    }
    ov.release(&cap.conn)?;
    for w in windows {
        cap.conn.unmap_window(w.id)?;
    }
    cap.conn.ungrab_keyboard(CURRENT_TIME)?;
    let pill = ui::Pill::new(&cap, fonts, scale)?;
    cap.overlay = Some(pill.win.sprite());
    cap.draw_pointer = last.pointer;
    let path = default_path("VIDEOS", &rec_prefix(), "mkv");
    let opts = RecOpts { fps: None, gpu: last.gpu, sound: Some((last.output, last.mic, app)) };
    record(&mut cap, &path, &opts, Some(pill), Some(windows[0].sprite()), target)?;
    let path = match last.mp4 {
        true => to_mp4(&path, &path.with_extension("mp4")).unwrap_or_else(|e| {
            notify(&tr!("Saved as MKV instead", "Se guardó como MKV", "MKV のまま保存しました"), &e.to_string(), None);
            path
        }),
        false => path,
    };
    notify(&tr!("Recording saved", "Grabación guardada", "録画を保存しました"), &tilde(&path), None);
    Ok(())
}

/// Grab until `gone` is off screen and the pill, if any, is composited as
/// drawn: from then on frames hold none of our UI. Gives up after a second.
fn settle(cap: &mut Capture, gone: Option<&Sprite>) -> Res<()> {
    let give_up = Instant::now() + Duration::from_secs(1);
    loop {
        cap.changed()?; // keeps the cursor current: the pill check skips its pixels
        cap.grab((0, cap.view.h as i32))?;
        let done = !gone.is_some_and(|s| cap.shows(s)) && (cap.overlay.is_none() || cap.overlay_seen);
        if done || Instant::now() > give_up {
            break;
        }
        std::thread::sleep(Duration::from_millis(4));
    }
    cap.invalidate(); // the encoder hasn't seen these grabs
    Ok(())
}

/// The pill's clicks and drags since the last call: pause or resume
/// (`paused` since when, `paused_for` in all) or stop, which sets STOP.
/// Without a pill, events nobody wants are dropped.
#[cfg(target_os = "linux")]
fn pump_pill(cap: &mut Capture, pill: &mut Option<Pill>, paused: &mut Option<Instant>, paused_for: &mut Duration) -> Res<()> {
    for ev in cap.take_events()? {
        let Some(p) = pill.as_mut() else { continue };
        match p.event(&cap.conn, &ev)? {
            PillEvent::TogglePause if paused.is_some() => {
                *paused_for += paused.take().unwrap().elapsed();
                p.set_paused(&cap.conn, false)?;
                cap.set_overlay(p.win.sprite());
                cap.retrack()?;
                settle(cap, None)?; // compositor must show this look before we remove it
            }
            PillEvent::TogglePause => {
                *paused = Some(Instant::now());
                p.set_paused(&cap.conn, true)?;
                cap.untrack()?; // repaints must not wake us while paused
            }
            PillEvent::Stop => {
                cap.conn.unmap_window(p.win.id)?;
                STOP.store(true, Relaxed);
            }
            PillEvent::None => {}
        }
    }
    if let Some(p) = pill.as_mut() {
        p.animate(&cap.conn)?;
        cap.move_overlay(p.win.x, p.win.y);
        if p.tick(&cap.conn)? {
            cap.set_overlay(p.win.sprite()); // the old look stays removable until it's off screen
        }
    }
    Ok(())
}

// The launcher (gui, shutter, the pill, the ui/select/shortcut modules) is X11
// code: elsewhere the command line works and the launcher says why it doesn't.

/// There is no launcher here, so never a pill.
#[cfg(not(target_os = "linux"))]
enum Pill {}

#[cfg(not(target_os = "linux"))]
impl Pill {
    fn gliding(&self) -> bool {
        match *self {}
    }
}

#[cfg(not(target_os = "linux"))]
fn pump_pill(_: &mut Capture, _: &mut Option<Pill>, _: &mut Option<Instant>, _: &mut Duration) -> Res<()> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn gui() -> Res<()> {
    let msg = tr!(
        "the launcher only runs on Linux (X11) for now: use `screenrec shot` or `screenrec rec`",
        "por ahora la interfaz solo funciona en Linux (X11): usa `screenrec shot` o `screenrec rec`",
        "ランチャーは今のところ Linux (X11) 専用です: `screenrec shot` か `screenrec rec` を使ってください"
    );
    Err(msg.into())
}

#[cfg(not(target_os = "linux"))]
fn install() -> Res<()> {
    Err(tr!("the launcher shortcut is GNOME-only for now", "por ahora el atajo de la interfaz es solo para GNOME", "ランチャーのショートカットは今のところ GNOME 専用です").into())
}

/// The H.264 encoder: NVENC on the GPU, another GPU encoder through ffmpeg
/// (VAAPI, Media Foundation, VideoToolbox), or x264 on the CPU.
enum Video {
    Gpu(Box<nvenc::Encoder>), // boxed: NVENC's function table is 2.6 KB
    GpuFfmpeg(x264::Encoder),
    Cpu(x264::Encoder),
}

/// An encoded frame: (timestamp ms, keyframe, Annex B).
type Unit = (u64, bool, Vec<u8>);

impl Video {
    /// If the GPU is wanted, the first that works of NVENC and this
    /// platform's GPU encoders in ffmpeg; else x264 on the CPU. A failing
    /// NVENC is reported, with what records instead.
    fn new(host: &[u8], w: usize, h: usize, fps: u32, gpu: bool) -> Res<Self> {
        let mut failed = None;
        if gpu && nvenc::available() {
            match nvenc::Encoder::new(host, w, h, fps) {
                Ok(e) => return Ok(Video::Gpu(Box::new(e))),
                Err(e) => failed = Some(e),
            }
        }
        let other = if gpu { x264::gpu(w, h, fps) } else { None };
        if let Some(e) = failed {
            let with = other.map_or_else(|| tr!("the CPU", "el CPU", "CPU"), |c| c.name.to_owned());
            notify(&tr!("NVENC unavailable, recording with {}", "NVENC no disponible: grabando con {}", "NVENC が使えないため {} で録画しています", with), &e.to_string(), None);
        }
        Ok(match other {
            Some(c) => Video::GpuFfmpeg(x264::Encoder::new(c, w, h, fps)?),
            None => Video::Cpu(x264::Encoder::new(&x264::X264, w, h, fps)?),
        })
    }

    fn upload(&mut self, host: &[u8], stride: usize, rows: (i32, i32)) -> Res<()> {
        match self {
            Video::Gpu(e) => e.upload(host, stride, rows),
            Video::GpuFfmpeg(e) | Video::Cpu(e) => {
                e.upload(host, stride, rows);
                Ok(())
            }
        }
    }

    /// Encode the frame stamped `ts`; returns the frames finished meanwhile
    /// (NVENC: this one; ffmpeg runs a frame or so behind). `idr` only steers NVENC.
    fn encode(&mut self, ts: u64, idr: bool) -> Res<Vec<Unit>> {
        match self {
            Video::Gpu(e) => {
                let mut unit = vec![];
                let key = e.encode(idr, &mut unit)?;
                Ok(vec![(ts, key, unit)])
            }
            Video::GpuFfmpeg(e) | Video::Cpu(e) => Ok(e.encode(ts)?.into_iter().map(|(t, u)| (t, mkv::is_keyframe(&u), u)).collect()),
        }
    }

    /// What is still in the encoder at the end.
    fn finish(&mut self) -> Vec<Unit> {
        match self {
            Video::Gpu(_) => vec![],
            Video::GpuFfmpeg(e) | Video::Cpu(e) => e.finish().into_iter().map(|(t, u)| (t, mkv::is_keyframe(&u), u)).collect(),
        }
    }
}

/// Remux a finished MKV to MP4 with the installed ffmpeg: video copied as is,
/// audio to AAC (Apple devices don't play Opus in MP4). Removes the MKV.
fn to_mp4(src: &Path, dst: &Path) -> Res<PathBuf> {
    let mut ff = std::process::Command::new("ffmpeg");
    ff.args(["-v", "error", "-y", "-i"]).arg(src);
    ff.args(["-map", "0", "-c:v", "copy", "-c:a", "aac", "-b:a", "160k", "-movflags", "+faststart"]).arg(dst);
    let missing = |_| tr!("MP4 needs ffmpeg ({})", "MP4 necesita ffmpeg ({})", "MP4 には ffmpeg が必要です ({})", desktop::GET_FFMPEG);
    if !ff.status().map_err(missing)?.success() {
        return Err(tr!("ffmpeg could not convert to MP4", "ffmpeg no pudo convertir a MP4", "ffmpeg で MP4 に変換できませんでした").into());
    }
    std::fs::remove_file(src)?;
    Ok(dst.to_owned())
}

/// Record `target` until STOP (or until the recorded window closes). In GUI
/// mode the pill is on screen (pause / stop / drag) and `gone` must be off
/// screen first.
fn record(cap: &mut Capture, path: &Path, opts: &RecOpts, mut pill: Option<Pill>, gone: Option<Sprite>, target: Target) -> Res<()> {
    for sig in [libc::SIGINT, libc::SIGTERM] {
        unsafe { libc::signal(sig, on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t) };
    }
    desktop::lower_priority();

    let (Target::Area(r) | Target::Window(_, r)) = target;
    let (w, h) = (((r.2 - r.0) & !1) as usize, ((r.3 - r.1) & !1) as usize); // 4:2:0 needs even sizes
    if w < 64 || h < 64 {
        return Err(tr!("the area is too small to record (minimum 64×64)", "el área es muy pequeña para grabar (mínimo 64×64)", "録画するには領域が小さすぎます (最小 64×64)").into());
    }
    // The area's rows come first in the capture buffer (same stride once the
    // region is set), so only that much is pinned for the GPU.
    let (w32, h32, settle_pill) = (w as i32, h as i32, pill.is_some());
    let setup = |cap: &mut Capture| -> Res<()> {
        match target {
            Target::Window(id, _) => {
                cap.overlay = None; // our UI is never in another window's pixmap
                cap.follow_window(id, (r.0, r.1, r.0 + w32, r.1 + h32))?;
            }
            Target::Area(_) => {
                cap.screen_readable()?;
                cap.track_changes()?;
                if settle_pill {
                    settle(cap, gone.as_ref())?;
                }
                cap.set_region(r.0, r.1, w32, h32);
            }
        }
        Ok(())
    };
    // On X11 the capture's setup (tracking, and settling until the compositor shows the
    // pill: ~30 ms) runs on a thread while this one builds the encoder (a CUDA context and
    // an NVENC session: ~200 ms), so the recording starts that much sooner. Elsewhere
    // there is no pill to wait for, so one after the other.
    #[cfg(target_os = "linux")]
    let mut enc = {
        let (host, len) = (cap.frame().as_ptr() as usize, w * h * 4);
        std::thread::scope(|s| -> Res<Video> {
            let capture = s.spawn(|| setup(cap).map_err(|e| e.to_string())); // a String crosses threads, the error type doesn't
            // SAFETY: the SHM mapping lives as long as `cap`; the encoder only pins these
            // bytes now and reads them later, after the thread above is joined.
            let host = unsafe { std::slice::from_raw_parts(host as *const u8, len) };
            let enc = Video::new(host, w, h, opts.fps.unwrap_or(60), opts.gpu);
            capture.join().map_err(|_| tr!("the capture could not be set up", "no se pudo preparar la captura", "キャプチャを準備できませんでした"))??;
            enc
        })?
    };
    #[cfg(not(target_os = "linux"))]
    let mut enc = {
        let enc = Video::new(&cap.frame()[..w * h * 4], w, h, opts.fps.unwrap_or(60), opts.gpu)?;
        setup(cap)?;
        enc
    };
    let fps = opts.fps.unwrap_or(if matches!(enc, Video::Cpu(_)) { 30 } else { 60 });
    let tick = Duration::from_secs(1) / fps;
    cap.set_tick(tick);
    // A video without sound beats no video: audio trouble only gets reported.
    let mut sound = opts.sound.and_then(|(output, mic, app)| {
        audio::Audio::start(output, mic, app).unwrap_or_else(|e| {
            notify(&tr!("Recording without sound", "Grabando sin sonido", "音声なしで録画しています"), &e.to_string(), None);
            None
        })
    });
    let mut mkv: Option<mkv::Mkv> = None;
    let (mut frames, mut last_ts, mut last_key) = (0u64, 0u64, 0u64);
    // Write a frame; the first one (with SPS/PPS) creates the file.
    let opus = sound.as_ref().map(|a| (a.opus_head(), a.pre_skip()));
    let put = |mkv: &mut Option<mkv::Mkv>, last_key: &mut u64, (t, key, unit): Unit| -> Res<()> {
        if key {
            *last_key = t;
        }
        match mkv {
            Some(m) => m.frame(t, key, &unit)?,
            None => {
                let mut m = mkv::Mkv::create(path, w, h, &unit, opus.clone())?;
                m.frame(t, key, &unit)?;
                *mkv = Some(m);
            }
        }
        Ok(())
    };
    eprintln!(
        "{}",
        tr!(
            "recording {w}x{h} at {fps} fps max. to {} (Ctrl+C to stop)",
            "grabando {w}x{h} a {fps} fps máx. en {} (Ctrl+C para terminar)",
            "{w}x{h} を最大 {fps} fps で {} に録画中 (Ctrl+C で停止)",
            path.display()
        )
    );

    let t0 = Instant::now();
    let mut last = t0 - tick;
    let (mut paused, mut paused_for) = (None::<Instant>, Duration::ZERO);
    // Audio frame (48 kHz) on the recording's timeline: wall clock minus pauses.
    let frame_of = |at: Instant, paused_for: Duration| {
        (at.saturating_duration_since(t0).as_micros() as i64 - paused_for.as_micros() as i64) * audio::RATE / 1_000_000
    };
    let res = (|| -> Res<()> {
        while !STOP.load(Relaxed) {
            pump_pill(cap, &mut pill, &mut paused, &mut paused_for)?;
            if let (Some(a), Some(m)) = (sound.as_mut(), mkv.as_mut()) {
                let until = frame_of(paused.unwrap_or_else(Instant::now), paused_for);
                for (ts, packet) in a.pump(|at| frame_of(at, paused_for), until, paused.is_some(), false)? {
                    m.audio(ts, &packet)?;
                }
            }
            if STOP.load(Relaxed) || cap.window_gone() {
                break;
            }
            if paused.is_some() {
                cap.wait(Some(Duration::from_millis(100)))?; // the pill's clicks wake us at once
                continue;
            }
            // At most `fps`, but otherwise capture the moment something changes:
            // every frame of content up to `fps` gets caught, none twice. Idle, the
            // loop sleeps as long as the capture allows (a tick while it has to poll
            // the cursor); a gliding pill needs every tick, audio a pump now and then.
            if let Some(d) = (last + tick).checked_duration_since(Instant::now()) {
                std::thread::sleep(d);
            }
            let mut idle = cap.poll_interval();
            if pill.as_ref().is_some_and(|p| p.gliding()) {
                idle = idle.min(tick);
            }
            if sound.is_some() {
                idle = idle.min(Duration::from_millis(100));
            }
            cap.wait(Some(idle))?;
            // Nothing changed -> no capture, no encode: a still screen costs ~0.
            let Some(rows) = cap.changed()? else { continue };
            last = Instant::now();
            let ts = (last - t0 - paused_for).as_millis() as u64;
            cap.grab(rows)?;
            enc.upload(cap.frame(), cap.view.w * 4, rows)?;
            for unit in enc.encode(ts, mkv.is_none() || ts - last_key >= KEYINT_MS)? {
                put(&mut mkv, &mut last_key, unit)?;
            }
            (frames, last_ts) = (frames + 1, ts);
        }
        Ok(())
    })();

    // Finalize even after an error: what got recorded is the user's data.
    let paused_now = paused.map_or(Duration::ZERO, |t| t.elapsed());
    let end = (t0.elapsed() - paused_for - paused_now).as_millis() as u64;
    // Repeat the last frame at the stop time so the video lasts until then,
    // and take what the encoder still holds.
    let mut tail = if end > last_ts && frames > 0 { enc.encode(end, false).unwrap_or_default() } else { vec![] };
    tail.extend(enc.finish());
    for unit in tail {
        put(&mut mkv, &mut last_key, unit)?;
    }
    if let Some(mut m) = mkv {
        if let Some(a) = sound.as_mut() {
            std::thread::sleep(Duration::from_millis(60)); // the last chunks still in parec
            for (ts, packet) in a.pump(|at| frame_of(at, paused_for), end as i64 * audio::RATE / 1000, paused.is_some(), true)? {
                m.audio(ts, &packet)?;
            }
        }
        m.finish(end)?;
        let mb = std::fs::metadata(path).map(|m| m.len() as f64 / 1e6).unwrap_or(0.0);
        eprintln!("{frames} frames, {:.1} s, {mb:.1} MB -> {}", end as f64 / 1000.0, path.display());
    }
    res
}

/// `path` with the home folder written `~`, for messages.
#[cfg(target_os = "linux")] // only the launcher's notifications use it
fn tilde(path: &Path) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    match path.strip_prefix(&home) {
        Ok(rest) if !home.is_empty() => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

fn shot_prefix() -> String {
    tr!("screenshot", "captura", "スクリーンショット")
}

fn rec_prefix() -> String {
    tr!("recording", "grabacion", "録画")
}

fn default_path(xdg_dir: &str, prefix: &str, ext: &str) -> PathBuf {
    let dir = desktop::user_dir(xdg_dir);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let (y, mo, d, h, mi, s) = desktop::local_time(now.as_secs());
    let ms = now.subsec_millis(); // two shots in one second must not overwrite each other
    dir.join(format!("{prefix}-{y}-{mo:02}-{d:02}_{h:02}-{mi:02}-{s:02}-{ms:03}.{ext}"))
}
