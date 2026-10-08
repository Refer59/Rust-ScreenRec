//! The text-recognition service, `screenrec ocrd`: one per user, started by the first client,
//! gone after five idle minutes. A screenshot or a recording hands it the pixels it already has
//! and goes on; the service recognizes them in order, one job at a time, and does what was
//! asked: a .txt next to the image or video, or the text on the clipboard, with a notification
//! for the launcher's users. Nothing of this runs, or is even loaded, unless text recognition
//! is on.

use crate::service::{self, Conn};
use crate::{Res, desktop, ocr, ocr_files};
use std::collections::{HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How often a recording reads the screen, in seconds: the default and the range.
pub const EVERY_DEFAULT: f32 = 2.0;
pub fn every_clamp(v: f32) -> f32 {
    if v.is_finite() { v.clamp(0.5, 5.0) } else { EVERY_DEFAULT }
}

const MAGIC: &[u8; 4] = b"OCR1";
const ENDPOINT: &str = "ocr";
const IDLE_EXIT: Duration = Duration::from_secs(300);
const MAX_PIXELS: u64 = 64 << 20; // 8K is 33 M

// ---------------------------------------------------------------------------------------------
// Protocol: a header, then one image (a screenshot) or images until the client hangs up (a
// recording, each image answered with one byte once it is done). Little-endian.
//   header: MAGIC, video u8, notify u8, clipboard u8, 0, path len u32, path (UTF-8)
//   image:  w u32, h u32, video ms u64, wall-clock s u64, w*h*4 bytes of BGRX
// `path` is the .txt to write (or, for the clipboard, the image it came from, for messages).

struct Header {
    video: bool,
    notify: bool,
    clipboard: bool,
    path: PathBuf,
}

impl Header {
    fn write(&self, w: &mut impl Write) -> std::io::Result<()> {
        let path = self.path.to_string_lossy();
        w.write_all(MAGIC)?;
        w.write_all(&[self.video as u8, self.notify as u8, self.clipboard as u8, 0])?;
        w.write_all(&(path.len() as u32).to_le_bytes())?;
        w.write_all(path.as_bytes())
    }

    fn read(r: &mut impl Read) -> Res<Header> {
        let mut head = [0u8; 12];
        r.read_exact(&mut head)?;
        if &head[..4] != MAGIC || head[7] != 0 {
            return Err("not a screenrec OCR request".into());
        }
        let len = u32::from_le_bytes(head[8..12].try_into().unwrap()) as usize;
        if len > 4096 {
            return Err("path too long".into());
        }
        let mut path = vec![0u8; len];
        r.read_exact(&mut path)?;
        Ok(Header { video: head[4] != 0, notify: head[5] != 0, clipboard: head[6] != 0, path: PathBuf::from(String::from_utf8(path)?) })
    }
}

struct Image {
    w: usize,
    h: usize,
    ms: u64,
    wall: u64,
    bgrx: Vec<u8>,
}

/// Send the image's rows (`stride` bytes apart) straight from the capture buffer: no copy here.
fn write_image(w: &mut impl Write, bgrx: &[u8], (iw, ih, stride): (usize, usize, usize), ms: u64) -> std::io::Result<()> {
    let wall = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    w.write_all(&(iw as u32).to_le_bytes())?;
    w.write_all(&(ih as u32).to_le_bytes())?;
    w.write_all(&ms.to_le_bytes())?;
    w.write_all(&wall.to_le_bytes())?;
    for y in 0..ih {
        w.write_all(&bgrx[y * stride..][..iw * 4])?;
    }
    w.flush()
}

fn read_image(r: &mut impl Read) -> Res<Option<Image>> {
    let mut head = [0u8; 24];
    match r.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None), // the client is done
        Err(e) => return Err(e.into()),
    }
    let u32_at = |i: usize| u32::from_le_bytes(head[i..i + 4].try_into().unwrap()) as usize;
    let u64_at = |i: usize| u64::from_le_bytes(head[i..i + 8].try_into().unwrap());
    let (w, h) = (u32_at(0), u32_at(4));
    if w == 0 || h == 0 || (w as u64) * (h as u64) > MAX_PIXELS {
        return Err(format!("bad image size {w}x{h}").into());
    }
    let mut bgrx = vec![0u8; w * h * 4];
    r.read_exact(&mut bgrx)?;
    Ok(Some(Image { w, h, ms: u64_at(8), wall: u64_at(16), bgrx }))
}

// ---------------------------------------------------------------------------------------------
// Clients.

/// Connect to the service, starting it if it isn't running. Fails fast if the files aren't
/// installed: nothing is started for nothing.
fn connect() -> Res<Conn> {
    ocr_files::files()?;
    if let Ok(c) = service::connect(ENDPOINT) {
        return Ok(c);
    }
    service::spawn_detached(&["ocrd"])?;
    let give_up = Instant::now() + Duration::from_secs(5);
    loop {
        match service::connect(ENDPOINT) {
            Ok(c) => return Ok(c),
            Err(e) if Instant::now() > give_up => return Err(tr!("the text recognition service didn't start: {}", "el servicio de reconocimiento de texto no arrancó: {}", "テキスト認識サービスが起動しませんでした: {}", e).into()),
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// A screenshot's pixels (w×h BGRX, rows `stride` apart) to the service: the text goes to
/// `<image without extension>.txt`, or to the clipboard; `notify` for a notification when done.
/// Returns as soon as the service has the pixels.
pub fn shot(bgrx: &[u8], w: usize, h: usize, stride: usize, image: &Path, clipboard: bool, notify: bool) -> Res<()> {
    let header = Header { video: false, notify, clipboard, path: if clipboard { image.to_owned() } else { image.with_extension("txt") } };
    let send = || -> Res<()> {
        let mut c = connect()?;
        header.write(&mut c)?;
        write_image(&mut c, bgrx, (w, h, stride), 0)?;
        Ok(())
    };
    // ponytail: a service that quit at the very moment we connected drops our bytes; one more try starts a fresh one.
    send().or_else(|_| send())
}

/// A recording's side of the service: frames offered every `every` seconds at most, one in
/// flight, each line of text appended once to `txt` with its time.
pub struct Video {
    pending: Option<Receiver<Result<Conn, String>>>, // connecting on a thread: the recording never waits
    conn: Option<Conn>,
    idle: Arc<AtomicBool>, // the service has answered the last frame
    dead: Arc<AtomicBool>, // ... or hung up
    every: Duration,
    last: Option<Instant>,
}

impl Video {
    pub fn start(txt: PathBuf, every: f32, notify: bool) -> Video {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let header = Header { video: true, notify, clipboard: false, path: txt };
            let _ = tx.send(connect().and_then(|mut c| header.write(&mut c).map(|()| c).map_err(Into::into)).map_err(|e| e.to_string()));
        });
        Video { pending: Some(rx), conn: None, idle: Arc::new(AtomicBool::new(true)), dead: Arc::new(AtomicBool::new(false)), every: Duration::from_secs_f32(every_clamp(every)), last: None }
    }

    /// A frame just recorded (w×h BGRX, rows `stride` apart, `ms` on the video's clock):
    /// sent if it is time and the service is free, else skipped; ~2 ms when sent.
    pub fn offer(&mut self, bgrx: &[u8], (w, h, stride): (usize, usize, usize), ms: u64) -> Res<()> {
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(Ok(conn)) => {
                    let (mut acks, idle, dead) = (conn.try_clone()?, self.idle.clone(), self.dead.clone());
                    std::thread::spawn(move || {
                        let mut b = [0u8; 1];
                        while acks.read_exact(&mut b).is_ok() {
                            idle.store(true, Relaxed);
                        }
                        dead.store(true, Relaxed);
                    });
                    (self.conn, self.pending) = (Some(conn), None);
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => return Err("the text recognition service couldn't be reached".into()),
            }
        }
        if self.dead.load(Relaxed) {
            return Err(tr!("the text recognition service hung up", "el servicio de reconocimiento de texto se cerró", "テキスト認識サービスが切断されました").into());
        }
        let due = self.last.is_none_or(|t| t.elapsed() >= self.every);
        if !due || !self.idle.load(Relaxed) {
            return Ok(());
        }
        let Some(conn) = self.conn.as_mut() else { return Ok(()) };
        write_image(conn, bgrx, (w, h, stride), ms)?;
        self.idle.store(false, Relaxed);
        self.last = Some(Instant::now());
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// The service.

/// A recording being followed: its file, what was already written, and the way back to it.
struct Session {
    txt: PathBuf,
    notify: bool,
    seen: Mutex<HashSet<String>>,
    ack: Mutex<Conn>,
}

enum Work {
    Shot(Header, Image),
    Frame(Arc<Session>, Image),
    End(Arc<Session>),
}

#[derive(Default)]
struct Queue {
    state: Mutex<(VecDeque<Work>, usize)>, // jobs in order; clients connected
    wake: Condvar,
}

impl Queue {
    fn push(&self, w: Work) {
        self.state.lock().unwrap().0.push_back(w);
        self.wake.notify_one();
    }

    fn clients(&self, delta: isize) {
        let mut g = self.state.lock().unwrap();
        g.1 = g.1.wrapping_add_signed(delta);
        self.wake.notify_one();
    }

    /// The next job; exits the process after IDLE_EXIT with no job and no client.
    fn next(&self) -> Work {
        let mut g = self.state.lock().unwrap();
        loop {
            if let Some(w) = g.0.pop_front() {
                return w;
            }
            let (guard, timeout) = self.wake.wait_timeout(g, IDLE_EXIT).unwrap();
            g = guard;
            if timeout.timed_out() && g.0.is_empty() && g.1 == 0 {
                std::process::exit(0);
            }
        }
    }
}

/// Serve until idle for five minutes; returns at once if another process already serves.
pub fn serve() -> Res<()> {
    let Some(mut listener) = service::listen(ENDPOINT)? else { return Ok(()) };
    desktop::lower_priority();
    let q = Arc::new(Queue::default());
    let worker = q.clone();
    std::thread::spawn(move || worker_loop(&worker));
    loop {
        let conn = listener.accept()?;
        let q = q.clone();
        q.clients(1);
        std::thread::spawn(move || {
            if let Err(e) = client(conn, &q) {
                eprintln!("ocrd: {e}");
            }
            q.clients(-1);
        });
    }
}

/// One client: a screenshot, or a recording's frames until it hangs up.
fn client(mut conn: Conn, q: &Queue) -> Res<()> {
    let header = Header::read(&mut conn)?;
    if !header.video {
        let image = read_image(&mut conn)?.ok_or("no image")?;
        q.push(Work::Shot(header, image));
        return Ok(());
    }
    let session = Arc::new(Session { txt: header.path, notify: header.notify, seen: Mutex::default(), ack: Mutex::new(conn.try_clone()?) });
    let res = (|| -> Res<()> {
        while let Some(image) = read_image(&mut conn)? {
            q.push(Work::Frame(session.clone(), image));
        }
        Ok(())
    })();
    q.push(Work::End(session));
    res
}

fn worker_loop(q: &Queue) {
    let mut engine: Option<ocr::Engine> = None;
    loop {
        let work = q.next();
        let engine = match &mut engine {
            Some(e) => e,
            slot => match ocr_files::files().and_then(|f| ocr::Engine::load(&f)) {
                Ok(e) => slot.insert(e),
                Err(e) => {
                    failed(&e.to_string());
                    continue;
                }
            },
        };
        if let Err(e) = run(engine, work) {
            failed(&e.to_string());
        }
    }
}

fn failed(e: &str) {
    eprintln!("ocrd: {e}");
    desktop::notify(&tr!("Couldn't read the text", "No se pudo leer el texto", "テキストを読み取れませんでした"), e, None);
}

fn run(engine: &ocr::Engine, work: Work) -> Res<()> {
    match work {
        Work::Shot(h, img) => {
            let lines = engine.recognize(&img.bgrx, img.w, img.h, img.w * 4)?;
            let text = ocr::text_of(&lines);
            let name = h.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if h.clipboard {
                if lines.is_empty() {
                    note(h.notify, &tr!("No text found", "No se encontró texto", "テキストが見つかりませんでした"), &name);
                    return Ok(());
                }
                desktop::copy_text(text.trim_end())?;
                let first = lines[0].text.chars().take(60).collect::<String>();
                let body = if lines.len() > 1 { tr!("{} and {} more lines", "{} y {} líneas más", "{} ほか {} 行", first, lines.len() - 1) } else { first };
                note(h.notify, &tr!("Text copied", "Texto copiado", "テキストをコピーしました"), &body);
                return Ok(());
            }
            std::fs::write(&h.path, &text)?;
            match lines.is_empty() {
                true => note(h.notify, &tr!("No text found", "No se encontró texto", "テキストが見つかりませんでした"), &name),
                false => note(h.notify, &tr!("Text saved", "Texto guardado", "テキストを保存しました"), &h.path.display().to_string()),
            }
            Ok(())
        }
        Work::Frame(s, img) => {
            let res = engine.recognize(&img.bgrx, img.w, img.h, img.w * 4).and_then(|lines| {
                let mut seen = s.seen.lock().unwrap();
                let mut out = String::new();
                for l in lines {
                    if seen.insert(l.text.split_whitespace().collect()) {
                        let (_, _, _, hh, mm, ss) = desktop::local_time(img.wall);
                        out.push_str(&format!("{} ({hh:02}:{mm:02}:{ss:02}) - {}\n", clock(img.ms), l.text));
                    }
                }
                if !out.is_empty() {
                    std::fs::OpenOptions::new().append(true).create(true).open(&s.txt)?.write_all(out.as_bytes())?;
                }
                Ok(())
            });
            let _ = s.ack.lock().unwrap().write_all(&[1]); // gone already: that's fine
            res
        }
        Work::End(s) => {
            let had_text = s.txt.is_file();
            if !had_text {
                std::fs::write(&s.txt, "")?; // "nothing was read" is an answer too
            }
            match had_text {
                true => note(s.notify, &tr!("Text saved", "Texto guardado", "テキストを保存しました"), &s.txt.display().to_string()),
                false => note(s.notify, &tr!("No text found", "No se encontró texto", "テキストが見つかりませんでした"), &s.txt.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
            }
            Ok(())
        }
    }
}

fn note(wanted: bool, title: &str, body: &str) {
    if wanted {
        desktop::notify(title, body, None);
    }
}

/// `ms` on the video's clock as HH:MM:SS.mmm.
fn clock(ms: u64) -> String {
    format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_and_image_round_trip() {
        let h = Header { video: true, notify: false, clipboard: true, path: PathBuf::from("/tmp/ñ 日本/clip.txt") };
        let mut bytes = Vec::new();
        h.write(&mut bytes).unwrap();
        let bgrx: Vec<u8> = (0..3 * 2 * 4 * 2).map(|i| i as u8).collect(); // 3x2, stride 24 of which 12 used
        write_image(&mut bytes, &bgrx, (3, 2, 24), 61_001).unwrap();
        let mut r = bytes.as_slice();
        let back = Header::read(&mut r).unwrap();
        assert!((back.video, back.notify, back.clipboard) == (true, false, true) && back.path == h.path);
        let img = read_image(&mut r).unwrap().unwrap();
        assert_eq!((img.w, img.h, img.ms), (3, 2, 61_001));
        assert_eq!(img.bgrx, [&bgrx[..12], &bgrx[24..36]].concat());
        assert!(img.wall > 1_700_000_000);
        assert!(read_image(&mut r).unwrap().is_none(), "a clean end");
        assert!(Header::read(&mut b"nope".as_slice()).is_err());
    }

    #[test]
    fn clock_and_interval() {
        assert_eq!(clock(3_661_007), "01:01:01.007");
        assert_eq!(clock(0), "00:00:00.000");
        assert_eq!((every_clamp(0.1), every_clamp(9.0), every_clamp(f32::NAN), every_clamp(2.5)), (0.5, 5.0, EVERY_DEFAULT, 2.5));
    }
}
