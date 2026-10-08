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
mod ocr;
mod ocr_files;
mod ocrd;
#[allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code, unused_imports, clippy::all)]
mod ort_sys;
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
use std::borrow::Cow;
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
  screenrec shot [file.png|.jpg] [--ocr] [--clip]
                                        full-screen screenshot; --ocr: its text to <file>.txt, or with --clip to the
                                        clipboard; --clip alone: the image to the clipboard
  screenrec rec [file.mkv|.mp4] [-r FPS] [--window ID] [--cpu] [--ocr] [--ocr-every S]
                                        record the screen (or a window) until Ctrl+C / SIGTERM
                                        (max FPS: 60 on the GPU, 30 without it; --cpu: no GPU even if there is one;
                                        --ocr: the text seen, with its times, to <file>.txt, read every S seconds: 0.5-5, 2 by default)
  screenrec install                     keyboard shortcut for the launcher ('-' if it has none yet) and the text recognition files",
        "uso:
  screenrec                             interfaz: captura o grabación (selección, pantalla o ventana)
  screenrec shot [archivo.png|.jpg] [--ocr] [--clip]
                                        captura de pantalla completa; --ocr: su texto a <archivo>.txt, o con --clip al
                                        portapapeles; --clip solo: la imagen al portapapeles
  screenrec rec [archivo.mkv|.mp4] [-r FPS] [--window ID] [--cpu] [--ocr] [--ocr-every S]
                                        graba la pantalla (o una ventana) hasta Ctrl+C / SIGTERM
                                        (máx. FPS: 60 con GPU, 30 sin ella; --cpu: sin GPU aunque haya;
                                        --ocr: el texto visto, con sus tiempos, a <archivo>.txt, leído cada S segundos: 0.5-5, 2 por defecto)
  screenrec install                     atajo de teclado para la interfaz ('-' si aún no tiene) y los archivos del reconocimiento de texto",
        "使い方:
  screenrec                             ランチャー: スクリーンショットまたは録画 (選択範囲、画面、ウィンドウ)
  screenrec shot [ファイル.png|.jpg] [--ocr] [--clip]
                                        画面全体のスクリーンショット。--ocr: そのテキストを <ファイル>.txt に、--clip も付ければ
                                        クリップボードに。--clip のみ: 画像をクリップボードに
  screenrec rec [ファイル.mkv|.mp4] [-r FPS] [--window ID] [--cpu] [--ocr] [--ocr-every S]
                                        Ctrl+C / SIGTERM まで画面 (またはウィンドウ) を録画
                                        (最大 FPS: GPU で 60、なしで 30。--cpu: GPU があっても使わない。
                                        --ocr: 映ったテキストを時刻付きで <ファイル>.txt に、S 秒ごとに読み取る: 0.5-5、既定 2)
  screenrec install                     ランチャーのキーボードショートカット (未設定なら '-') とテキスト認識のファイル"
    )
}

/// Wall time between forced keyframes (seek granularity).
const KEYINT_MS: u64 = 5000;

static STOP: AtomicBool = AtomicBool::new(false);
/// A recording has its encoder and its first frame is next: what the launcher waits
/// for before it fades out of a recording that includes it.
static RECORDING: AtomicBool = AtomicBool::new(false);

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
        Some("shot") => shot(&args[1..]),
        Some("rec") => rec(&args[1..]),
        Some("install") => install(),
        Some("ocrd") => ocrd::serve(), // the text recognition service, started by its first client
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

/// Point our GNOME shortcut at this executable ('-' unless one was already picked), and
/// install the text recognition files. Without GNOME (a server, CI) the shortcut is skipped.
#[cfg(target_os = "linux")]
fn install() -> Res<()> {
    let accel = shortcut::get().unwrap_or_else(|| "minus".into());
    match shortcut::set(&accel) {
        Ok(()) => println!("{}", tr!("launcher shortcut: {}", "atajo de la interfaz: {}", "ランチャーのショートカット: {}", shortcut::pretty(&accel))),
        Err(e) => println!("{}", tr!("launcher shortcut: not set, this isn't GNOME ({})", "atajo de la interfaz: sin configurar, esto no es GNOME ({})", "ランチャーのショートカット: 未設定、GNOME ではありません ({})", e)),
    }
    ocr_files::install(&mut std::io::stdout())
}

fn shot(args: &[String]) -> Res<()> {
    let (mut out, mut ocr, mut clip) = (None, false, false);
    for a in args {
        match a.as_str() {
            "--ocr" => ocr = true,
            "--clip" => clip = true,
            p if !p.starts_with("--") => out = Some(PathBuf::from(p)),
            p => return Err(tr!("unknown option {}", "opción desconocida {}", "不明なオプション {}", p).into()),
        }
    }
    if ocr {
        ocr_files::files()?; // a clear error before anything is captured
    }
    let mut cap = Capture::new()?;
    freeze(&mut cap)?;
    let path = out.unwrap_or_else(|| default_path("PICTURES", &shot_prefix(), "png"));
    let (w, h) = (cap.sw, cap.sh);
    let img = save_image(cap.frame(), w, (0, 0, w as i32, h as i32), None, &path)?;
    println!("{}", path.display());
    // The text (or the image) is wanted after the file: the same bytes go on, no re-read.
    if ocr {
        ocrd::shot(&img, w, h, w * 4, &path, clip, false)?;
    } else if clip {
        desktop::copy_image(&img, w, h, w * 4)?;
    }
    Ok(())
}

fn rec(args: &[String]) -> Res<()> {
    let (mut opts, mut path, mut window) = (RecOpts { fps: None, gpu: true, sound: None, ocr: None, ui: false }, None, None);
    let (mut ocr, mut every) = (false, ocrd::EVERY_DEFAULT);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-r" => opts.fps = Some(it.next().and_then(|v| v.parse().ok()).filter(|f| (1..=240).contains(f)).ok_or(tr!("-r expects 1..240", "-r espera 1..240", "-r には 1..240 を指定してください"))?),
            "--cpu" => opts.gpu = false,
            "--ocr" => ocr = true,
            "--ocr-every" => {
                let secs: f32 = it.next().and_then(|v| v.parse().ok()).ok_or(tr!("--ocr-every expects seconds (0.5 to 5)", "--ocr-every espera segundos (0.5 a 5)", "--ocr-every には秒数を指定してください (0.5〜5)"))?;
                (ocr, every) = (true, ocrd::every_clamp(secs));
            }
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
    if ocr {
        ocr_files::files()?; // a clear error before anything is recorded
        opts.ocr = Some((path.with_extension("txt"), every));
    }
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
/// where encoding is what costs), GPU or not, the sound (output, microphone,
/// the pid whose sound "Window" means), and text recognition (the .txt to
/// write, how often to read the screen in seconds), and whether our own UI
/// stays in the video (the pill isn't taken out of the frames).
struct RecOpts {
    fps: Option<u32>,
    gpu: bool,
    sound: Option<(Output, bool, Option<u32>)>,
    ocr: Option<(PathBuf, f32)>,
    ui: bool,
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
/// Returns the image it wrote: w×h BGRX, rows w·4 bytes apart.
fn save_image<'a>(screen: &'a [u8], sw: usize, r: Rect, cursor: Option<&Sprite>, path: &Path) -> Res<Cow<'a, [u8]>> {
    let (w, h) = ((r.2 - r.0) as usize, (r.3 - r.1) as usize);
    let rows = || (r.1..r.3).map(|y| &screen[(y as usize * sw + r.0 as usize) * 4..][..w * 4]);
    // Full-width rows are already one contiguous image; a cut-out or a
    // drawn-in pointer needs its own copy.
    let img = if cursor.is_none() && w == sw {
        Cow::Borrowed(&screen[r.1 as usize * sw * 4..r.3 as usize * sw * 4])
    } else {
        let mut own = Vec::with_capacity(w * h * 4);
        rows().for_each(|row| own.extend_from_slice(row));
        if let Some(c) = cursor {
            frame::draw(&mut own, View { w, h, x0: r.0, y0: r.1 }, c);
        }
        Cow::Owned(own)
    };
    if path.extension().is_some_and(|e| e == "jpg" || e == "jpeg") {
        let jpg = jpeg_encoder::Encoder::new_file(path, 90)?;
        jpg.encode(&img, w as u16, h as u16, jpeg_encoder::ColorType::Bgra)?; // the 4th byte is ignored
        return Ok(img);
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
    Ok(img)
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
    /// Copy screenshots to the clipboard.
    clip: bool,
    /// Recognize text; only an area's (Área mode), see `text`.
    ocr: bool,
    /// Seconds between the frames a recording reads text from: at most one per interval.
    ocr_every: f32,
    /// Capture the whole screen with the launcher in it, whatever the mode.
    with_ui: bool,
}

fn last_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    let config = std::env::var("XDG_CONFIG_HOME").unwrap_or(home + "/.config");
    Path::new(&config).join("screenrec/last")
}

#[cfg(target_os = "linux")]
impl Last {
    fn load(sw: i32, sh: i32) -> Self {
        Self::parse(&std::fs::read_to_string(last_path()).unwrap_or_default(), sw, sh)
    }

    fn parse(text: &str, sw: i32, sh: i32) -> Self {
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
        let ocr_every = f.get(15).and_then(|v| v.parse::<f32>().ok()).filter(|v| v.is_finite()).map_or(ui::EVERY_DEFAULT, ui::every_clamp);
        // Older files end at 12 (clip and ocr off), at 14 (the default interval) or at 15 (the UI left out).
        Last { mode, record: flag(1), pointer: flag(2), sel, output, mic: flag(8), mp4: flag(9), jpg: flag(10), gpu, clip: flag(13), ocr: flag(14), ocr_every, with_ui: flag(16) }
    }

    fn save(&self) {
        let path = last_path();
        let (m, (x0, y0, x1, y1), o) = (self.mode, self.sel, self.output);
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let (rec, ptr, mic, mp4, jpg, gpu, clip, ocr, every, ui) = (self.record, self.pointer, self.mic, self.mp4, self.jpg, self.gpu, self.clip, self.ocr, self.ocr_every, self.with_ui);
        let lang = i18n::chosen().map_or("-".into(), |l| format!("{l:?}")); // "-": the locale's
        let _ = std::fs::write(path, format!("{m:?} {rec} {ptr} {x0} {y0} {x1} {y1} {o:?} {mic} {mp4} {jpg} {gpu} {lang} {clip} {ocr} {every} {ui}\n"));
    }

    /// Whether the capture's text gets recognized: the switch is on and the mode is Área
    /// (not overruled by the UI, which makes it the whole screen).
    fn text(&self) -> bool {
        self.ocr && self.mode == Mode::Selection && !self.with_ui
    }
}

#[cfg(all(test, target_os = "linux"))]
#[test]
fn last_reads_older_files() {
    let old = Last::parse("Selection true false 1 2 300 400 System true false true false Es\n", 1920, 1080);
    assert!((old.record, old.mic, old.jpg, old.gpu, old.clip, old.ocr) == (true, true, true, false, false, false));
    let v6 = Last::parse("Selection false false 1 2 300 400 None false false false true - true true\n", 1920, 1080);
    assert!((v6.clip, v6.ocr, v6.text(), v6.ocr_every) == (true, true, true, 2.0));
    assert!(!Last { mode: Mode::Screen, ..v6 }.text(), "text is read from an area only");
    assert_eq!(old.ocr_every, 2.0);
    // The interval, clamped to 0.5..5 s; what isn't a number is the default.
    let every = |v: &str| Last::parse(&format!("Selection false false 1 2 300 400 None false false false true - true true {v}\n"), 1920, 1080).ocr_every;
    assert_eq!(["2.5", "0.5", "5", "0.1", "9", "-3", "1.25", "nan", "inf", "x", ""].map(every), [2.5, 0.5, 5.0, 0.5, 5.0, 0.5, 1.3, 2.0, 2.0, 2.0, 2.0]);
}

#[cfg(all(test, target_os = "linux"))]
#[test]
fn last_with_ui() {
    // Files without it (v7 and older) leave the UI out.
    let v7 = Last::parse("Selection false false 1 2 300 400 None false false false true - true true 2.5\n", 1920, 1080);
    assert!(!v7.with_ui && v7.text());
    assert!(!Last::parse("Selection true false 1 2 300 400 System true false true false Es\n", 1920, 1080).with_ui);
    let v8 = Last::parse("Selection false false 1 2 300 400 None false false false true - true true 2.5 true\n", 1920, 1080);
    assert!(v8.with_ui && v8.ocr && v8.ocr_every == 2.5);
    assert!(!v8.text(), "no text read with the UI in: it's the whole screen");
    assert!(!Last::parse("Selection false false 1 2 300 400 None false false false true - true true 2.5 false\n", 1920, 1080).with_ui);
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

    /// What the key types into a number: a digit, '.' or ','. The keypad's digits are
    /// its second column with NumLock (Mod2) on; Shift takes the main keys' (AZERTY digits).
    fn typed(&self, code: u8, state: u16) -> Option<char> {
        let col = |i: usize| if i < self.per { self.syms.get(code.saturating_sub(self.min) as usize * self.per + i).copied().unwrap_or(0) } else { 0 };
        let keypad = |s: u32| (0xffac..=0xffb9).contains(&s);
        let sym = if state & 0x10 != 0 && keypad(col(1)) || state & 1 != 0 && !keypad(col(1)) { col(1) } else { col(0) };
        match sym {
            0x30..=0x39 | 0x2c | 0x2e => char::from_u32(sym),
            0xffb0..=0xffb9 => char::from_u32(sym - 0xffb0 + 0x30), // KP_0..KP_9
            0xffac | 0xffae => Some('.'),                          // KP_Separator, KP_Decimal
            _ => None,
        }
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
#[cfg(target_os = "linux")]
const KEY_BACKSPACE: u32 = 0xff08;

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
    for sig in [libc::SIGINT, libc::SIGTERM] {
        unsafe { libc::signal(sig, on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t) }; // the shortcut again: fade out, then quit
    }
    let reduced = std::thread::spawn(shortcut::animations_off); // a gsettings call: runs while the screen freezes
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
    (st.ocr, st.with_ui) = (last.ocr, last.with_ui);
    let mut window = name_of(&cap, hovered); // the name of the window Window mode would take, for the badge
    let ((px, py), (mx, my)) = ui::place(sw, sh, scale);
    let mask = EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION | EventMask::LEAVE_WINDOW;
    // The size badge sits under the panel in the stack (made first): the selection's size,
    // or the window's name and size; Screen mode has none.
    let gap = (14.0 * scale).round() as i32; // clear of the corner brackets
    let badge_for = |(r, name): &(Rect, Option<String>), with_ui: bool| {
        let c = ui::badge(r.2 - r.0, r.3 - r.1, name.as_deref(), font(), scale);
        (ui::badge_pos(*r, (c.w as i32, c.h as i32), (sw, sh), gap, ui::panel_rect(sw, sh, scale, with_ui)), c)
    };
    let badge_want = |last: &Last, w: Option<(Rect, u32)>, name: &Option<String>| match last.mode {
        Mode::Selection => Some((last.sel, None)),
        Mode::Window => w.map(|(r, _)| (r, name.clone())),
        Mode::Screen => None,
    };
    let mut badge_at = badge_want(&last, hovered, &window); // what it shows, if mapped
    let ((bx, by), bc) = badge_for(&badge_at.clone().unwrap_or((last.sel, None)), last.with_ui);
    let mut badge = ui::Win::new(&cap, bx, by, bc, EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION)?;
    let mut panel = ui::Win::new(&cap, px, py, ui::panel(&st), mask)?;
    let shortcut_now = || shortcut::get().map_or(tr!("none", "ninguno", "なし"), |a| shortcut::pretty(&a));
    let mut set = ui::SettingsState::new(fonts(), None, scale);
    // The shortcut (two gsettings runs), the NVIDIA probe (a driver dlopen) and the
    // render wait for the gear: none of them is needed to show the panel.
    (set.output, set.mic, set.pointer) = (audio::OUTPUTS.iter().position(|&o| o == last.output).unwrap_or(0), last.mic, last.pointer);
    (set.mp4, set.jpg, set.gpu, set.clip, set.ocr_every) = (last.mp4, last.jpg, last.gpu, last.clip, last.ocr_every);
    set.with_ui = last.with_ui;
    let mut modal = ui::Win::new(&cap, mx, my, ui::Canvas::new(ui::SW, ui::SH, scale), mask)?; // drawn and mapped by the gear
    // The tooltip of Área's switch, shown while it's hovered or focused.
    let mut tip = ui::Win::new(&cap, 0, 0, ui::Canvas::new(1, 1, scale), EventMask::NO_EVENT)?;
    let mut tip_text: Option<String> = None;
    // The caption under the panel while the capture includes the UI: click-through like the badge.
    let mut caption = ui::Win::new(&cap, 0, 0, ui::Canvas::new(1, 1, scale), EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION)?;
    let mut caption_a = 0.0f32; // the opacity it was drawn at (-1: redraw it)
    // Everything fades in together: the dimmed screen, the badge, the panel.
    let ours = [ov.win, badge.id, panel.id, modal.id, tip.id, caption.id];
    let mut fade = ui::Fade::new(&cap)?;
    fade.apply(&cap.conn, &ours)?; // transparent before it's mapped
    ui::set_reduced_motion(reduced.join().unwrap_or(false));
    fade.go(1.0);
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
    let (pid, mid, bid, cid) = (panel.id, modal.id, badge.id, caption.id);
    let mut thru = false; // a press began in a transparent margin: the drag belongs to the overlay
    loop {
        cap.wait((moving || moving_set || fade.busy()).then_some(Duration::from_millis(8)))?; // the next animation frame, else sleep until an event
        fade.apply(&cap.conn, &ours)?;
        if STOP.load(Relaxed) {
            return fade_out(&cap.conn, &mut fade, &ours);
        }
        let (mut redraw, mut reshape, mut restyle) = (false, false, false); // panel, overlay, settings
        macro_rules! close_settings {
            () => {
                set.commit();
                (st.settings_open, set.capturing, redraw, pfocus) = (false, false, true, Hit::Settings);
                cap.conn.unmap_window(mid)?;
            };
        }
        for mut ev in cap.take_events()? {
            // The panels' transparent margins and the badge are click-through: the overlay gets those
            // events, or the panel where the settings' margin overhangs it (its close button).
            let body = |id: u32, x: i16, y: i16| id != bid && id != cid && (id != pid || ui::panel_body_has(scale, x, y)) && (id != mid || ui::settings_body_has(scale, x, y));
            let under = |id: u32, rx: i16, ry: i16| {
                let (x, y) = ((rx as i32 - px) as i16, (ry as i32 - py) as i16);
                if id == mid && ui::panel_body_has(scale, x, y) { (pid, x, y) } else { (ov.win, rx, ry) }
            };
            match &mut ev {
                Event::ButtonPress(e) => {
                    ring = false;
                    set.commit(); // a click anywhere leaves the interval's field (on it, it starts again)
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
                    let h = ui::panel_hit(scale, e.event_x, e.event_y, st.mode == Mode::Selection);
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
                    press = ui::panel_hit(scale, e.event_x, e.event_y, st.mode == Mode::Selection).map(|h| (Press::Panel(h), false));
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
                    let (sym, typed) = (keys.sym(e.detail), keys.typed(e.detail, e.state.into()));
                    let nav = sym == KEY_TAB || KEYS_PREV.contains(&sym) || KEYS_NEXT.contains(&sym);
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
                    } else if let Some(t) = set.typing.as_mut().filter(|_| typed.is_some() || !nav) {
                        // The interval's field takes the keys, none reach the launcher; Tab and the arrows leave it.
                        match typed {
                            Some(ch) => t.key(ch),
                            None if sym == KEY_BACKSPACE => t.back(),
                            None if sym == KEY_ESCAPE => set.typing = None, // back to the value it had
                            None if KEY_ENTERS[..2].contains(&sym) => set.commit(),
                            None => {}
                        }
                        restyle = true;
                        false
                    } else if let Some(ch) = typed.filter(|_| st.settings_open && ring && sfocus == SetHit::Every(1) && !set.off(sfocus)) {
                        let mut t = ui::Typing::new(set.ocr_every); // a digit on the focused field types over it
                        t.key(ch);
                        (set.typing, restyle) = (Some(t), true);
                        false
                    } else if sym == KEY_ESCAPE && st.settings_open {
                        close_settings!();
                        false
                    } else if sym == KEY_ESCAPE {
                        return fade_out(&cap.conn, &mut fade, &ours);
                    } else if nav {
                        let back = if sym == KEY_TAB { u16::from(e.state) & 1 != 0 } else { KEYS_PREV.contains(&sym) }; // Shift is bit 1
                        if set.typing.is_some() {
                            (ring, restyle) = (true, true); // leaving the field keeps what was typed, and moves on
                            set.commit();
                        }
                        if ring && st.settings_open {
                            sfocus = ui::step(&ui::SETTINGS_ORDER, sfocus, back, |h| set.off(h));
                        } else if ring {
                            pfocus = ui::step(&ui::PANEL_ORDER, pfocus, back, |h| h == Hit::Ocr && (st.mode != Mode::Selection || st.with_ui));
                        }
                        ring = true; // the first press only shows where the focus is
                        if st.settings_open && sfocus == SetHit::Every(1) {
                            set.typing = Some(ui::Typing::new(set.ocr_every)); // arriving at the field: ready to type over its value
                        }
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
                        Hit::Close => return fade_out(&cap.conn, &mut fade, &ours),
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
                        Hit::Ocr if st.with_ui => {} // off while the UI is in the capture
                        Hit::Ocr => {
                            (st.ocr, last.ocr) = (!st.ocr, !st.ocr);
                            last.save();
                        }
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
                        SetHit::WithUi => {
                            set.with_ui = !set.with_ui;
                            (st.with_ui, redraw, badge_at, reshape) = (set.with_ui, true, None, true); // the badge keeps clear of the caption, or not
                        }
                        SetHit::Clip if !set.record => set.clip = !set.clip,
                        SetHit::Clip => {} // photos only: off limits on video
                        SetHit::VideoFormat(i) => set.mp4 = i == 1,
                        SetHit::Gpu => set.gpu = !set.gpu,
                        SetHit::ImageFormat(i) => set.jpg = i == 1,
                        SetHit::Shortcut => set.capturing = true,
                        SetHit::Every(_) if set.off(h) => {} // no text read from a video: it doesn't matter
                        SetHit::Every(1) => (set.typing, sfocus) = (Some(ui::Typing::new(set.ocr_every)), h),
                        SetHit::Every(i) => set.ocr_every = ui::every_step(set.ocr_every, i == 2),
                        SetHit::Lang(i) => {
                            i18n::set(i18n::LANGS[i]);
                            ((st.font, st.bold), (set.font, set.bold)) = (fonts(), fonts());
                            (set.cjk, set.shortcut, redraw, reshape, badge_at, caption_a) = (cjk(), shortcut_now(), true, true, None, -1.0); // the badge and the caption, in the new font
                        }
                    }
                    // Settings stick at once, also when the launcher is then closed.
                    (last.pointer, last.output, last.mic) = (set.pointer, audio::OUTPUTS[set.output], set.mic);
                    (last.mp4, last.jpg, last.gpu, last.clip, last.ocr_every, last.with_ui) = (set.mp4, set.jpg, set.gpu, set.clip, set.ocr_every, set.with_ui);
                    last.save();
                }
                None => {}
            }
            if shoot {
                (last.mode, last.record, last.ocr_every) = (st.mode, st.record, set.ocr_every);
                let target = match (last.mode, picked.or(hovered)) {
                    _ if last.with_ui => Some(Target::Area((0, 0, sw, sh))), // the launcher is on all of it
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
                return shutter(cap, &mut ov, (&[&panel, &modal, &badge, &tip, &caption], &mut fade), target, &last, &cursor, app, (fonts(), scale));
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
                        let ((x, y), c) = badge_for(b, st.with_ui);
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
        if pfocus == Hit::Ocr && (st.mode != Mode::Selection || st.with_ui) {
            pfocus = Hit::Mode(st.mode); // the switch went with Área, or is off with the UI in
        }
        let (rec, text, area) = (st.record, st.ocr && st.mode == Mode::Selection && !st.with_ui, st.mode == Mode::Selection);
        (restyle, set.record, set.text, set.area) = (restyle || (rec, text, area) != (set.record, set.text, set.area), rec, text, area);
        if set.off(sfocus) {
            sfocus = ui::step(&ui::SETTINGS_ORDER, sfocus, true, |h| set.off(h)); // its control went off limits (the clipboard on video, ...)
        }
        if last.ocr_every != set.ocr_every {
            last.ocr_every = set.ocr_every;
            last.save();
        }
        let f = (ring && !st.settings_open).then_some(pfocus);
        (redraw, st.focus) = (redraw || f != st.focus, f);
        let f = ring.then_some(sfocus);
        (restyle, set.focus) = (restyle || f != set.focus, f);
        st.sync();
        // The caption fades with the panel's `ui` tween: drawn while it runs, gone at 0.
        let a = st.ui_shown();
        if a != caption_a {
            if a > 0.0 {
                let c = ui::ui_caption(font(), scale, a);
                let (x, y) = ui::caption_pos(sw, sh, scale, (c.w as i32, c.h as i32));
                caption.reset(&cap.conn, x, y, c)?;
                if caption_a <= 0.0 {
                    caption.show(&cap.conn)?;
                }
            } else {
                cap.conn.unmap_window(cid)?;
            }
            caption_a = a;
        }
        if redraw || moving || st.busy() {
            panel.redraw(&cap.conn, ui::panel(&st))?;
        }
        moving = st.busy();
        // Área's switch explains itself while it's hovered or focused.
        let want = (st.mode == Mode::Selection && (st.hover == Some(Hit::Ocr) || st.focus == Some(Hit::Ocr))).then(|| ui::ocr_tip(st.ocr, st.record, set.clip, st.with_ui));
        if want != tip_text {
            if let Some(text) = &want {
                let c = ui::tooltip(text, font(), scale, 1.0);
                let (x, y) = ui::tooltip_pos(sw, sh, scale, (c.w as i32, c.h as i32));
                tip.reset(&cap.conn, x, y, c)?;
                tip.show(&cap.conn)?;
            } else {
                cap.conn.unmap_window(tip.id)?;
            }
            tip_text = want;
        }
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
fn shutter(mut cap: Capture, ov: &mut select::Overlay, (windows, fade): (&[&ui::Win], &mut ui::Fade), target: Option<Target>, last: &Last, cursor: &Sprite, app: Option<u32>, (fonts, scale): ((Option<&ab_glyph::FontVec>, Option<&ab_glyph::FontVec>), f32)) -> Res<()> {
    let ours: Vec<u32> = std::iter::once(ov.win).chain(windows.iter().map(|w| w.id)).collect();
    let Some(target) = target else { return fade_out(&cap.conn, fade, &ours) }; // Window mode with no window picked
    let (Target::Area(r) | Target::Window(_, r)) = target;
    last.save();
    let text = last.text(); // recognize the capture's text
    if !last.record {
        // The file never waits for the fade: it's cut from the frozen screen while the launcher fades out.
        // With the UI in, it's the screen as it is now instead, the launcher on it, taken before the fade.
        let live = if last.with_ui { Some(freeze(&mut cap)?) } else { None };
        let cursor = live.as_ref().unwrap_or(cursor);
        let path = default_path("PICTURES", &shot_prefix(), if last.jpg { "jpg" } else { "png" });
        let (frame, sw) = (cap.frame(), cap.sw);
        let (w, h) = ((r.2 - r.0) as usize, (r.3 - r.1) as usize);
        return std::thread::scope(|s| {
            // Errors cross the thread as Strings; the copy's own failure doesn't undo the saved file.
            let saved = s.spawn(|| -> Result<(Option<String>, Option<String>), String> {
                let img = save_image(frame, sw, r, last.pointer.then_some(cursor), &path).map_err(|e| e.to_string())?;
                // With the text recognized, the clipboard gets the text instead: the same bytes
                // go to the service, which answers with its own notification when it has read them.
                let read_failed = text.then(|| ocrd::shot(&img, w, h, w * 4, &path, last.clip, true).err().map(|e| e.to_string())).flatten();
                Ok((if last.clip && !text { desktop::copy_image(&img, w, h, w * 4).err().map(|e| e.to_string()) } else { None }, read_failed))
            });
            fade_out(&cap.conn, fade, &ours)?;
            let (copy_failed, read_failed) = saved.join().map_err(|_| "the screenshot could not be saved")??;
            let title = match last.clip && !text && copy_failed.is_none() {
                true => tr!("Screenshot saved and copied", "Captura guardada y copiada", "スクリーンショットを保存してコピーしました"),
                false => tr!("Screenshot saved", "Captura guardada", "スクリーンショットを保存しました"),
            };
            notify(&title, &tilde(&path), Some(&path));
            if let Some(e) = copy_failed {
                notify(&tr!("Couldn't copy the screenshot", "No se pudo copiar la captura", "スクリーンショットをコピーできませんでした"), &e, None);
            }
            if let Some(e) = read_failed {
                notify(&tr!("Couldn't read the text", "No se pudo leer el texto", "テキストを読み取れませんでした"), &e, None);
            }
            Ok(())
        });
    }
    let path = default_path("VIDEOS", &rec_prefix(), "mkv");
    let ui = last.with_ui;
    let opts = RecOpts { fps: None, gpu: last.gpu, sound: Some((last.output, last.mic, app)), ocr: text.then(|| (path.with_extension("txt"), last.ocr_every)), ui };
    cap.draw_pointer = last.pointer;
    if matches!(target, Target::Window(..)) || ui {
        // A window is recorded from its own pixmap, where the launcher never shows; with the
        // UI in, the launcher is meant to be in the video, and so is the pill. Either way the
        // recording starts at once, and the launcher fades out meanwhile on a thread with its
        // own connection (this one is the recording's): with the UI in, from its first frame.
        cap.conn.ungrab_keyboard(CURRENT_TIME)?;
        RECORDING.store(false, Relaxed);
        if !ui {
            fade.go(0.0);
        }
        let pill = ui::Pill::new(&cap, fonts, scale)?;
        let res = std::thread::scope(|s| {
            let fading = s.spawn(|| -> Result<(), String> {
                let mut fade_out = || -> Res<()> {
                    let (conn, _) = x11rb::connect(None)?;
                    let give_up = Instant::now() + Duration::from_secs(3); // the recording failed to start: go anyway
                    while ui && !RECORDING.load(Relaxed) && !STOP.load(Relaxed) && Instant::now() < give_up {
                        std::thread::sleep(Duration::from_millis(4));
                    }
                    fade.go(0.0);
                    loop {
                        let done = !fade.busy();
                        fade.apply(&conn, &ours)?;
                        conn.flush()?;
                        if done {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(8));
                    }
                    for &w in &ours {
                        conn.unmap_window(w)?;
                    }
                    // A round trip before this connection closes: a server busy with the recording's
                    // grabs sees the hang-up first and drops what it hasn't read, the launcher stays up.
                    conn.get_input_focus()?.reply()?;
                    Ok(())
                };
                fade_out().map_err(|e| e.to_string()) // a String crosses threads, the error type doesn't
            });
            let rec = record(&mut cap, &path, &opts, Some(pill), None, target);
            RECORDING.store(true, Relaxed); // failed before its first frame (no encoder): the launcher goes now, not in 3 s
            let _ = fading.join(); // done long ago
            rec
        });
        ov.release(&cap.conn)?;
        res?;
    } else {
        // An area is cut from the screen, so it waits until the launcher is gone: none of
        // it may end up in the video.
        fade.hurry();
        fade_out(&cap.conn, fade, &ours)?;
        if STOP.load(Relaxed) {
            return Ok(()); // the shortcut again while it faded: that's a cancel
        }
        ov.release(&cap.conn)?;
        for w in windows {
            cap.conn.unmap_window(w.id)?;
        }
        let pill = ui::Pill::new(&cap, fonts, scale)?;
        cap.overlay = Some(pill.win.sprite());
        record(&mut cap, &path, &opts, Some(pill), Some(windows[0].sprite()), target)?;
    }
    if !path.exists() {
        return Ok(()); // stopped before the first frame
    }
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

/// Fade the launcher out and return once it's invisible. The keyboard goes
/// back to the desktop at once, so typing right after Esc isn't lost.
#[cfg(target_os = "linux")]
fn fade_out(conn: &impl Connection, fade: &mut ui::Fade, ours: &[u32]) -> Res<()> {
    conn.ungrab_keyboard(CURRENT_TIME)?;
    fade.go(0.0);
    loop {
        let done = !fade.busy();
        fade.apply(conn, ours)?;
        conn.flush()?;
        if done {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(8));
    }
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
/// Without a pill, events nobody wants are dropped. `ui`: the pill stays in
/// the frames, so capture.rs isn't told about its new looks.
#[cfg(target_os = "linux")]
fn pump_pill(cap: &mut Capture, pill: &mut Option<Pill>, paused: &mut Option<Instant>, paused_for: &mut Duration, ui: bool) -> Res<()> {
    for ev in cap.take_events()? {
        let Some(p) = pill.as_mut() else { continue };
        match p.event(&cap.conn, &ev)? {
            PillEvent::TogglePause if paused.is_some() => {
                *paused_for += paused.take().unwrap().elapsed();
                p.set_paused(&cap.conn, false)?;
                if !ui {
                    cap.set_overlay(p.win.sprite());
                }
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
        if p.tick(&cap.conn)? && !ui {
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
fn pump_pill(_: &mut Capture, _: &mut Option<Pill>, _: &mut Option<Instant>, _: &mut Duration, _: bool) -> Res<()> {
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

/// The text recognition files; the launcher (and its shortcut) is Linux-only for now.
#[cfg(not(target_os = "linux"))]
fn install() -> Res<()> {
    println!("{}", tr!("the launcher shortcut is GNOME-only for now", "por ahora el atajo de la interfaz es solo para GNOME", "ランチャーのショートカットは今のところ GNOME 専用です"));
    ocr_files::install(&mut std::io::stdout())
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
    let ui = opts.ui;

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
    // Text recognition reads frames at the end of the pipeline: the service connects on its
    // own thread, gets a frame every `every` seconds at most, one at a time, and never holds
    // up a frame; if it goes away, the recording goes on without it.
    let mut text = opts.ocr.as_ref().map(|(txt, every)| ocrd::Video::start(txt.clone(), *every, pill.is_some()));
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
    RECORDING.store(true, Relaxed);
    let mut last = t0 - tick;
    let (mut paused, mut paused_for) = (None::<Instant>, Duration::ZERO);
    // Audio frame (48 kHz) on the recording's timeline: wall clock minus pauses.
    let frame_of = |at: Instant, paused_for: Duration| {
        (at.saturating_duration_since(t0).as_micros() as i64 - paused_for.as_micros() as i64) * audio::RATE / 1_000_000
    };
    let res = (|| -> Res<()> {
        while !STOP.load(Relaxed) {
            pump_pill(cap, &mut pill, &mut paused, &mut paused_for, ui)?;
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
            if let Some(t) = text.as_mut()
                && let Err(e) = t.offer(cap.frame(), (w, h, cap.view.w * 4), ts)
            {
                eprintln!("{e}");
                if pill.is_some() {
                    notify(&tr!("Recording without text recognition", "Grabando sin reconocimiento de texto", "テキスト認識なしで録画しています"), &e.to_string(), None);
                }
                text = None;
            }
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
