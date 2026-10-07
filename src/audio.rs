//! Recording audio: PulseAudio streams read with `parec` (the whole system
//! output, the streams of the recorded app, the microphone), mixed against
//! the wall clock and encoded to Opus. Nothing is re-routed, so what you hear
//! doesn't change.

use crate::Res;
use crate::dylib::{self, sym};
use std::collections::VecDeque;
use std::ffi::{CStr, c_void};
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, OnceLock};
use std::sync::mpsc::{Receiver, channel};
use std::time::Instant;

pub const RATE: i64 = 48000;
const FRAME: usize = 960; // 20 ms: one Opus packet
// A mic chunk was heard about parec's buffering (--latency-msec=20) before it
// arrives. Output (system, app) is captured as the server mixes it and heard
// about one sink latency later, which roughly cancels that buffering: lag 0.
const MIC_LAG: i64 = RATE / 50;
const DELAY: i64 = RATE / 10; // mix this far behind the wall clock, so every source has delivered
const RESYNC: i64 = RATE / 20; // a source drifting 50 ms off the wall clock gets re-anchored

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Output {
    None,
    System,
    Window,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // the launcher's sound picker
pub const OUTPUTS: [Output; 3] = [Output::None, Output::System, Output::Window];

/// libopus, loaded at run time: without it we record without sound.
struct Opus {
    encoder_create: unsafe extern "C" fn(fs: i32, channels: i32, application: i32, error: *mut i32) -> *mut c_void,
    encoder_ctl: unsafe extern "C" fn(st: *mut c_void, request: i32, ...) -> i32,
    encode: unsafe extern "C" fn(st: *mut c_void, pcm: *const i16, frame_size: i32, data: *mut u8, max_data_bytes: i32) -> i32,
    encoder_destroy: unsafe extern "C" fn(st: *mut c_void),
}

#[cfg(target_os = "linux")]
const OPUS_LIBS: &[&CStr] = &[c"libopus.so.0"];
#[cfg(target_os = "macos")]
const OPUS_LIBS: &[&CStr] = &[c"libopus.0.dylib", c"/opt/homebrew/lib/libopus.0.dylib", c"/usr/local/lib/libopus.0.dylib"];
#[cfg(windows)]
const OPUS_LIBS: &[&CStr] = &[c"opus.dll", c"libopus-0.dll"];

fn opus() -> Res<&'static Opus> {
    static O: OnceLock<Option<Opus>> = OnceLock::new();
    let load = || unsafe {
        let lib = OPUS_LIBS.iter().find_map(|n| dylib::open(n))?;
        Some(Opus {
            encoder_create: sym(lib, c"opus_encoder_create")?,
            encoder_ctl: sym(lib, c"opus_encoder_ctl")?,
            encode: sym(lib, c"opus_encode")?,
            encoder_destroy: sym(lib, c"opus_encoder_destroy")?,
        })
    };
    let missing = || tr!("sound needs the Opus library (libopus)", "el sonido necesita la biblioteca Opus (libopus)", "音声には Opus ライブラリ (libopus) が必要です");
    O.get_or_init(load).as_ref().ok_or_else(|| missing().into())
}
const OPUS_APPLICATION_AUDIO: i32 = 2049;
const OPUS_SET_BITRATE: i32 = 4002;
const OPUS_GET_LOOKAHEAD: i32 = 4027;

/// One `parec`, read by its own thread in 20 ms chunks.
struct Source {
    child: Child,
    rx: Receiver<(Instant, Vec<i16>)>,
    next: Option<i64>, // frame where its next chunk goes
    stream: Option<u32>, // the sink input it follows, for app audio
    lag: i64,            // frames between when it was heard and when it arrives
}

impl Drop for Source {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parec(target: &str, lag: i64) -> Res<Source> {
    let args = ["--raw", "--format=s16le", "--rate=48000", "--channels=2", "--latency-msec=20", "--client-name=screenrec", target];
    let missing = |_| tr!("sound needs parec (PulseAudio tools)", "el sonido necesita parec (herramientas de PulseAudio)", "音声には parec (PulseAudio のツール) が必要です");
    let mut child = Command::new("parec").args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(missing)?;
    let mut out = child.stdout.take().ok_or(tr!("parec has no stdout", "parec sin stdout", "parec の stdout がありません"))?;
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let mut b = [0u8; FRAME * 4];
        while out.read_exact(&mut b).is_ok() {
            let pcm = b.as_chunks::<2>().0.iter().map(|s| i16::from_le_bytes([s[0], s[1]])).collect();
            if tx.send((Instant::now(), pcm)).is_err() {
                break;
            }
        }
    });
    Ok(Source { child, rx, next: None, stream: None, lag })
}

/// Whether process `pid` is `ancestor` or was started by it (Chromium-style
/// apps play audio from a child process).
fn descends(mut pid: u32, ancestor: u32) -> bool {
    for _ in 0..32 {
        if pid == ancestor {
            return true;
        }
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        // "pid (comm) state ppid ...": comm may hold spaces and parens.
        match stat.rsplit_once(')').and_then(|(_, r)| r.split_whitespace().nth(1)).and_then(|p| p.parse().ok()) {
            Some(ppid) if ppid > 1 => pid = ppid,
            _ => return false,
        }
    }
    false
}

/// Sink inputs (playback streams) and the pid that owns each.
fn sink_inputs() -> Vec<(u32, u32)> {
    let out = Command::new("pactl").env("LC_ALL", "C").args(["list", "sink-inputs"]).output();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    text.split("Sink Input #")
        .skip(1)
        .filter_map(|b| {
            let idx = b.split_whitespace().next()?.parse().ok()?;
            let pid = b.lines().find_map(|l| l.trim().strip_prefix("application.process.id = \"")?.strip_suffix('"')?.parse().ok())?;
            Some((idx, pid))
        })
        .collect()
}

/// `pactl subscribe`, read by a thread that raises `new` whenever a playback
/// stream appears. Without it (no pactl, or it died) the caller rescans
/// every second instead.
struct Watch {
    child: Child,
    new: Arc<AtomicBool>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn watch() -> Option<Watch> {
    let args = ["subscribe"];
    let mut child = Command::new("pactl").env("LC_ALL", "C").args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let out = child.stdout.take()?;
    let new = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&new);
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if line.starts_with("Event 'new' on sink-input") {
                flag.store(true, Relaxed);
            }
        }
    });
    Some(Watch { child, new })
}

/// Add `pcm` (interleaved stereo) into `mix`, whose first frame is `base`,
/// starting at frame `start`. Frames before `base` were already encoded: too
/// late, dropped.
fn mix_in(mix: &mut VecDeque<i32>, base: i64, start: i64, pcm: &[i16]) {
    let n = (pcm.len() / 2) as i64;
    for f in (base - start).max(0)..n {
        let rel = start + f - base;
        if rel > 2 * RATE {
            break; // a bogus anchor must not grow the buffer without bound
        }
        let i = rel as usize * 2;
        if mix.len() < i + 2 {
            mix.resize(i + 2, 0);
        }
        mix[i] += pcm[f as usize * 2] as i32;
        mix[i + 1] += pcm[f as usize * 2 + 1] as i32;
    }
}

pub struct Audio {
    sources: Vec<Source>,
    app: Option<u32>, // pid whose playback streams we follow
    watch: Option<Watch>, // tells us when to look for new streams of it
    scanned: Instant,
    mix: VecDeque<i32>, // interleaved stereo sums, starting at frame `base`
    base: i64,          // next frame to encode, counted from the start
    opus: &'static Opus,
    enc: *mut c_void,
    pre_skip: u16,
}

impl Drop for Audio {
    fn drop(&mut self) {
        unsafe { (self.opus.encoder_destroy)(self.enc) };
    }
}

impl Audio {
    /// Start capturing: the system output or the playback of process `app`
    /// (and its children), plus the default microphone. None if all are off.
    pub fn start(output: Output, mic: bool, app: Option<u32>) -> Res<Option<Self>> {
        if output == Output::None && !mic {
            return Ok(None);
        }
        if cfg!(not(target_os = "linux")) {
            return Err(tr!("recording sound isn't available on this system yet", "grabar sonido aún no está disponible en este sistema", "この環境ではまだ音声を録音できません").into());
        }
        let opus = opus()?;
        let mut err = 0;
        let enc = unsafe { (opus.encoder_create)(RATE as i32, 2, OPUS_APPLICATION_AUDIO, &mut err) };
        if enc.is_null() {
            return Err(format!("opus_encoder_create: error {err}").into());
        }
        let mut lookahead = 0i32;
        unsafe {
            (opus.encoder_ctl)(enc, OPUS_SET_BITRATE, 128_000i32);
            (opus.encoder_ctl)(enc, OPUS_GET_LOOKAHEAD, &mut lookahead as *mut i32);
        }
        let mut a = Audio {
            sources: vec![],
            app: if output == Output::Window { app } else { None },
            watch: if output == Output::Window { watch() } else { None }, // subscribed before the first scan: no gap
            scanned: Instant::now(),
            mix: VecDeque::new(),
            base: 0,
            opus,
            enc,
            pre_skip: lookahead as u16,
        };
        if output == Output::System {
            a.sources.push(parec("--device=@DEFAULT_MONITOR@", 0)?);
        }
        if mic {
            a.sources.push(parec("--device=@DEFAULT_SOURCE@", MIC_LAG)?);
        }
        a.follow_app()?;
        Ok(Some(a))
    }

    /// Opus identification header: the Matroska track's CodecPrivate.
    pub fn opus_head(&self) -> Vec<u8> {
        let mut h = b"OpusHead".to_vec();
        h.extend([1, 2]); // version, channels
        h.extend(self.pre_skip.to_le_bytes());
        h.extend((RATE as u32).to_le_bytes());
        h.extend([0, 0, 0]); // output gain, channel mapping family
        h
    }

    /// Encoder delay in samples (Matroska CodecDelay).
    pub fn pre_skip(&self) -> u16 {
        self.pre_skip
    }

    /// Capture the app's playback streams we aren't capturing yet (apps open
    /// new ones whenever they like: a browser per video, a game at a scene change).
    fn follow_app(&mut self) -> Res<()> {
        self.scanned = Instant::now();
        let Some(app) = self.app else { return Ok(()) };
        for (idx, pid) in sink_inputs() {
            if (pid == app || descends(pid, app)) && !self.sources.iter().any(|s| s.stream == Some(idx)) {
                let mut s = parec(&format!("--monitor-stream={idx}"), 0)?;
                s.stream = Some(idx);
                self.sources.push(s);
            }
        }
        Ok(())
    }

    /// Take in what the sources captured and return the Opus packets, as
    /// (timestamp ms, data), for everything up to frame `until` (minus a short
    /// delay unless `flush`). `frame_of` maps a wall-clock instant to a frame
    /// on the recording's timeline; while `paused` audio is thrown away.
    pub fn pump(&mut self, frame_of: impl Fn(Instant) -> i64, until: i64, paused: bool, flush: bool) -> Res<Vec<(u64, Vec<u8>)>> {
        if self.app.is_some() {
            // Rescan when a stream appeared; every second if pactl isn't there to say so
            // (and every 10 s anyway, in case an event got lost).
            if self.watch.as_mut().is_some_and(|w| !matches!(w.child.try_wait(), Ok(None))) {
                self.watch = None; // pactl subscribe died: back to polling
            }
            let since = self.scanned.elapsed().as_secs();
            let rescan = match &self.watch {
                Some(w) => w.new.swap(false, Relaxed) || since >= 10,
                None => since >= 1,
            };
            if rescan {
                self.follow_app()?;
            }
        }
        self.sources.retain_mut(|s| !matches!(s.child.try_wait(), Ok(Some(_)))); // stream gone, parec ended
        for s in &mut self.sources {
            while let Ok((at, pcm)) = s.rx.try_recv() {
                if paused {
                    s.next = None;
                    continue;
                }
                let n = (pcm.len() / 2) as i64;
                let wall = frame_of(at) - n - s.lag; // where the wall clock puts this chunk
                let start = s.next.filter(|p| (p - wall).abs() < RESYNC).unwrap_or(wall);
                s.next = Some(start + n);
                mix_in(&mut self.mix, self.base, start, &pcm);
            }
        }
        let until = if flush { until } else { until - DELAY };
        let mut packets = vec![];
        let (mut pcm, mut out) = ([0i16; FRAME * 2], [0u8; 4000]);
        while self.base + FRAME as i64 <= until {
            for v in pcm.iter_mut() {
                *v = self.mix.pop_front().unwrap_or(0).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            }
            let n = unsafe { (self.opus.encode)(self.enc, pcm.as_ptr(), FRAME as i32, out.as_mut_ptr(), out.len() as i32) };
            if n < 0 {
                return Err(format!("opus_encode: error {n}").into());
            }
            packets.push(((self.base * 1000 / RATE) as u64, out[..n as usize].to_vec()));
            self.base += FRAME as i64;
        }
        Ok(packets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixing() {
        let mut mix = VecDeque::new();
        mix_in(&mut mix, 100, 102, &[1, 2, 3, 4]); // two frames at 102..104
        mix_in(&mut mix, 100, 98, &[10, 10, 20, 20, 30, 30, 40, 40, 50, 50]); // 98..103: first two too late
        assert_eq!(Vec::from(mix), [30, 30, 40, 40, 51, 52, 3, 4]);
    }

    #[test]
    fn opus_loads_at_run_time() {
        let Ok(opus) = opus() else { return }; // not installed: sound is optional
        let mut err = 0;
        let enc = unsafe { (opus.encoder_create)(RATE as i32, 2, OPUS_APPLICATION_AUDIO, &mut err) };
        assert!(!enc.is_null(), "error {err}");
        let mut lookahead = 0i32;
        let mut out = [0u8; 4000];
        let n = unsafe {
            (opus.encoder_ctl)(enc, OPUS_GET_LOOKAHEAD, &mut lookahead as *mut i32);
            (opus.encode)(enc, [0i16; FRAME * 2].as_ptr(), FRAME as i32, out.as_mut_ptr(), out.len() as i32)
        };
        unsafe { (opus.encoder_destroy)(enc) };
        assert!(lookahead > 0 && n > 0, "lookahead {lookahead}, packet {n}");
    }
}
