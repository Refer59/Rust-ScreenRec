//! H.264 in an ffmpeg process: x264 (ultrafast, zerolatency) on the CPU, or
//! the platform's GPU encoder (VAAPI, Media Foundation, VideoToolbox) when
//! NVENC isn't there. It gets I420 converted here, and only for the rows that
//! changed: ffmpeg's own BGRX->YUV conversion costs as much as the encoding.

use crate::Res;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

/// An H.264 encoder in ffmpeg: a name for messages, what sets it up (before
/// the input) and what picks it. Every one must open each frame with an access
/// unit delimiter (see `next_unit`) and make no B-frames: frames come back in
/// the order they went in.
pub struct Codec {
    pub name: &'static str,
    setup: &'static [&'static str],
    args: &'static [&'static str],
}

pub const X264: Codec = Codec {
    name: "x264",
    setup: &[],
    args: &["-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency", "-crf", "23", "-g", "300", "-x264-params", "aud=1"],
};

// h264_vaapi's own `-aud 1` breaks P-frames with ffmpeg 4.4 and Intel's iHD
// driver; the h264_metadata filter adds the delimiters on any encoder.
#[cfg(target_os = "linux")]
const VAAPI: &[&str] = &["-vf", "format=nv12,hwupload", "-c:v", "h264_vaapi", "-qp", "22", "-g", "300", "-bf", "0", "-bsf:v", "h264_metadata=aud=insert"];

/// The GPU encoders worth trying, best first. VAAPI (Intel, AMD): the
/// display's GPU first, then the first two render nodes.
#[cfg(target_os = "linux")]
const GPU: &[Codec] = &[
    Codec { name: "VAAPI", setup: &["-init_hw_device", "vaapi=va", "-filter_hw_device", "va"], args: VAAPI },
    Codec { name: "VAAPI", setup: &["-init_hw_device", "vaapi=va:/dev/dri/renderD128", "-filter_hw_device", "va"], args: VAAPI },
    Codec { name: "VAAPI", setup: &["-init_hw_device", "vaapi=va:/dev/dri/renderD129", "-filter_hw_device", "va"], args: VAAPI },
];

// ponytail: fixed 16 Mbit/s for the encoders without a constant-quality mode
// everywhere; scale it with the area if 4K recordings look soft.
#[cfg(windows)]
const GPU: &[Codec] = &[Codec {
    name: "Media Foundation",
    setup: &[],
    args: &["-c:v", "h264_mf", "-hw_encoding", "1", "-b:v", "16M", "-g", "300", "-bf", "0", "-bsf:v", "dump_extra,h264_metadata=aud=insert"],
}];

#[cfg(target_os = "macos")]
const GPU: &[Codec] = &[Codec {
    name: "VideoToolbox",
    setup: &[],
    args: &["-c:v", "h264_videotoolbox", "-realtime", "1", "-b:v", "16M", "-g", "300", "-bf", "0", "-bsf:v", "dump_extra,h264_metadata=aud=insert"],
}];

/// The first GPU encoder that works here at this size, if any.
pub fn gpu(w: usize, h: usize, fps: u32) -> Option<&'static Codec> {
    GPU.iter().find(|c| works(c, w, h, fps))
}

/// Whether ffmpeg encodes two black frames with `c` (~0.1 s; a stuck driver
/// gets 5 s).
fn works(c: &Codec, w: usize, h: usize, fps: u32) -> bool {
    let input = format!("color=c=black:s={w}x{h}:r={fps}");
    let mut ff = Command::new("ffmpeg");
    ff.args(["-v", "error"]).args(c.setup).args(["-f", "lavfi", "-i", &input, "-frames:v", "2"]).args(c.args).args(["-f", "null", "-"]);
    let Ok(mut child) = ff.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else { return false };
    let give_up = Instant::now() + Duration::from_secs(5);
    while Instant::now() < give_up {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    false
}

pub struct Encoder {
    child: Child,
    stdin: Option<ChildStdin>,
    units: Receiver<Vec<u8>>, // access units (Annex B), one per frame, in order
    pending: VecDeque<u64>,   // stamps of the frames sent and not back yet
    yuv: Vec<u8>,
    w: usize,
    h: usize,
}

/// Where the access unit after the first one starts (each opens with a
/// delimiter NAL, see `Codec`; NAL payloads never contain 00 00 01).
fn next_unit(b: &[u8]) -> Option<usize> {
    (4..b.len().saturating_sub(3)).find(|&i| b[i..i + 4] == [0, 0, 1, 9]).map(|i| if b[i - 1] == 0 { i - 1 } else { i })
}

impl Encoder {
    pub fn new(codec: &Codec, w: usize, h: usize, fps: u32) -> Res<Self> {
        let (size, rate) = (format!("{w}x{h}"), fps.to_string());
        #[rustfmt::skip]
        let (input, output) = (
            ["-f", "rawvideo", "-pix_fmt", "yuv420p", "-s", &size, "-r", &rate, "-i", "-"],
            ["-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-color_range", "tv",
             "-vsync", "0", "-flush_packets", "1", "-f", "h264", "-"],
        );
        let mut child = Command::new("ffmpeg")
            .args(["-v", "error"])
            .args(codec.setup)
            .args(input)
            .args(codec.args)
            .args(output)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| tr!("recording without a GPU needs ffmpeg ({})", "grabar sin GPU necesita ffmpeg ({})", "GPU なしの録画には ffmpeg が必要です ({})", crate::desktop::GET_FFMPEG))?;
        let (stdin, mut out) = (child.stdin.take(), child.stdout.take().ok_or(tr!("ffmpeg has no stdout", "ffmpeg sin stdout", "ffmpeg の stdout がありません"))?);
        let (tx, units) = channel();
        std::thread::spawn(move || {
            let (mut buf, mut chunk) = (Vec::new(), vec![0u8; 1 << 16]);
            while let Ok(n @ 1..) = out.read(&mut chunk) {
                buf.extend_from_slice(&chunk[..n]);
                while let Some(cut) = next_unit(&buf) {
                    let rest = buf.split_off(cut);
                    if tx.send(std::mem::replace(&mut buf, rest)).is_err() {
                        return;
                    }
                }
            }
            if !buf.is_empty() {
                let _ = tx.send(buf);
            }
        });
        Ok(Encoder { child, stdin, units, pending: VecDeque::new(), yuv: vec![0; w * h * 3 / 2], w, h })
    }

    /// Convert rows [y0, y1) of the BGRX frame (`stride` bytes per row) to
    /// I420, BT.709 limited range, the same colours NVENC produces.
    pub fn upload(&mut self, host: &[u8], stride: usize, (y0, y1): (i32, i32)) {
        let (w, h) = (self.w, self.h);
        let (y0, y1) = (y0.max(0) as usize & !1, ((y1.max(0) as usize + 1) & !1).min(h));
        if y0 >= y1 {
            return;
        }
        let (ys, uv) = self.yuv.split_at_mut(w * h);
        let (us, vs) = uv.split_at_mut(w * h / 4);
        let (c0, c1) = (y0 / 2 * w / 2, y1 / 2 * w / 2);
        convert(&host[y0 * stride..], stride, w, &mut ys[y0 * w..y1 * w], &mut us[c0..c1], &mut vs[c0..c1]);
    }

    /// Send the frame stamped `ts`. Returns the frames x264 finished meanwhile,
    /// with their stamps: it runs alongside, a frame or so behind.
    pub fn encode(&mut self, ts: u64) -> Res<Vec<(u64, Vec<u8>)>> {
        let stdin = self.stdin.as_mut().ok_or(tr!("ffmpeg already exited", "ffmpeg ya terminó", "ffmpeg はすでに終了しています"))?;
        stdin.write_all(&self.yuv).map_err(|_| tr!("ffmpeg stopped encoding (does it have libx264?)", "ffmpeg dejó de codificar (¿tiene libx264?)", "ffmpeg のエンコードが停止しました (libx264 はありますか?)"))?;
        self.pending.push_back(ts);
        let mut done = vec![];
        while let Ok(unit) = self.units.try_recv() {
            done.extend(self.pending.pop_front().map(|t| (t, unit)));
        }
        Ok(done)
    }

    /// End of the recording: close the input and collect what is left.
    pub fn finish(&mut self) -> Vec<(u64, Vec<u8>)> {
        drop(self.stdin.take());
        let mut done = vec![];
        while let Ok(unit) = self.units.recv() {
            done.extend(self.pending.pop_front().map(|t| (t, unit)));
        }
        let _ = self.child.wait();
        done
    }
}

/// BGRX rows (from `src`, `stride` apart) into the I420 planes `ys`, `us`,
/// `vs`, which cover the same row pairs. Pixels are read as u32 so the loops
/// vectorize with plain SSE2: luma walks both rows of a pair in one loop (two
/// independent streams), chroma sums each 2x2 block with red and blue side by
/// side in one u32 and does the matrix in i16.
fn convert(src: &[u8], stride: usize, w: usize, ys: &mut [u8], us: &mut [u8], vs: &mut [u8]) {
    let rows = ys.chunks_exact_mut(2 * w).zip(us.chunks_exact_mut(w / 2)).zip(vs.chunks_exact_mut(w / 2));
    for (((ypair, ur), vr), j) in rows.zip((0..).step_by(2)) {
        let (top, bot) = (&src[j * stride..][..w * 4], &src[(j + 1) * stride..][..w * 4]);
        let (ya, yb) = ypair.split_at_mut(w);
        for (((p, q), y), z) in top.as_chunks::<4>().0.iter().zip(bot.as_chunks::<4>().0).zip(ya).zip(yb) {
            (*y, *z) = (luma(*p), luma(*q));
        }
        for (((a, b), u), v) in top.as_chunks::<8>().0.iter().zip(bot.as_chunks::<8>().0).zip(ur).zip(vr) {
            let px = |s: &[u8; 8], i: usize| u32::from_le_bytes([s[i], s[i + 1], s[i + 2], s[i + 3]]);
            let (p0, p1, q0, q1) = (px(a, 0), px(a, 4), px(b, 0), px(b, 4));
            // 2x2 averages; red and blue summed side by side in one u32
            let rb = (p0 & 0xFF00FF) + (p1 & 0xFF00FF) + (q0 & 0xFF00FF) + (q1 & 0xFF00FF) + 0x20002;
            let g = ((p0 >> 8 & 0xFF) + (p1 >> 8 & 0xFF) + (q0 >> 8 & 0xFF) + (q1 >> 8 & 0xFF) + 2) >> 2;
            let (r, g, bl) = ((rb >> 18) as i16, g as i16, (rb >> 2 & 0x3FFF) as i16);
            *u = ((-26 * r - 86 * g + 112 * bl + 128) >> 8) as u8 ^ 0x80; // ^0x80: +128
            *v = ((112 * r - 102 * g - 10 * bl + 128) >> 8) as u8 ^ 0x80;
        }
    }
}

/// (47R + 157G + 16B + 128) / 256 + 16, the +16 folded into the rounding term.
#[inline(always)]
fn luma(p: [u8; 4]) -> u8 {
    let p = u32::from_le_bytes(p);
    let br = p & 0xFF00FF;
    ((47 * (br >> 16) + 157 * (p >> 8 & 0xFF) + 16 * (br & 0xFFFF) + 4224) >> 8) as u8
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_bt709_limited() {
        let quiet = Command::new(std::env::current_exe().unwrap()).arg("--list").stdout(Stdio::null()).stderr(Stdio::null()).spawn();
        let child = quiet.unwrap(); // any process that exits: `true` isn't there on Windows
        let mut e = Encoder { child, stdin: None, units: channel().1, pending: VecDeque::new(), yuv: vec![0; 6], w: 2, h: 2 };
        e.upload(&[0, 0, 255, 0].repeat(4), 8, (0, 2)); // pure red
        assert_eq!(e.yuv, [63, 63, 63, 63, 102, 240]);
        e.upload(&[255; 16], 8, (0, 2));
        assert_eq!(e.yuv, [235, 235, 235, 235, 128, 128]);
    }

    /// The implementation before the luma/chroma loops were reworked.
    fn convert_ref(src: &[u8], stride: usize, w: usize, ys: &mut [u8], us: &mut [u8], vs: &mut [u8]) {
        let rows = ys.chunks_exact_mut(2 * w).zip(us.chunks_exact_mut(w / 2)).zip(vs.chunks_exact_mut(w / 2));
        for (((ypair, ur), vr), j) in rows.zip((0..).step_by(2)) {
            let (top, bot) = (&src[j * stride..][..w * 4], &src[(j + 1) * stride..][..w * 4]);
            let (ya, yb) = ypair.split_at_mut(w);
            for (row, out) in [(top, ya), (bot, yb)] {
                for (p, y) in row.as_chunks::<4>().0.iter().zip(out) {
                    let p = u32::from_le_bytes(*p);
                    *y = ((47 * (p >> 16 & 0xFF) + 157 * (p >> 8 & 0xFF) + 16 * (p & 0xFF) + 128) >> 8) as u8 + 16;
                }
            }
            for (((a, b), u), v) in top.as_chunks::<8>().0.iter().zip(bot.as_chunks::<8>().0).zip(ur).zip(vr) {
                let px = |s: &[u8; 8], i: usize| u32::from_le_bytes([s[i], s[i + 1], s[i + 2], s[i + 3]]);
                let (p0, p1, q0, q1) = (px(a, 0), px(a, 4), px(b, 0), px(b, 4));
                // 2x2 averages; red and blue summed side by side in one u32
                let rb = (p0 & 0xFF00FF) + (p1 & 0xFF00FF) + (q0 & 0xFF00FF) + (q1 & 0xFF00FF) + 0x20002;
                let g = ((p0 >> 8 & 0xFF) + (p1 >> 8 & 0xFF) + (q0 >> 8 & 0xFF) + (q1 >> 8 & 0xFF) + 2) >> 2;
                let (r, g, bl) = ((rb >> 18) as i16, g as i16, (rb >> 2 & 0x3FFF) as i16);
                *u = ((-26 * r - 86 * g + 112 * bl + 128) >> 8) as u8 ^ 0x80; // ^0x80: +128
                *v = ((112 * r - 102 * g - 10 * bl + 128) >> 8) as u8 ^ 0x80;
            }
        }
    }

    #[test]
    fn convert_matches_reference() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        for (w, h, pad) in [(64, 8, 0), (130, 6, 12), (2, 2, 4), (1920, 4, 64)] {
            let stride = w * 4 + pad;
            let src: Vec<u8> = (0..stride * h)
                .map(|_| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    (seed >> 56) as u8
                })
                .collect();
            let planes = |f: fn(&[u8], usize, usize, &mut [u8], &mut [u8], &mut [u8])| {
                let (mut y, mut u, mut v) = (vec![0; w * h], vec![0; w * h / 4], vec![0; w * h / 4]);
                f(&src, stride, w, &mut y, &mut u, &mut v);
                (y, u, v)
            };
            assert!(planes(convert) == planes(convert_ref), "{w}x{h} stride {stride}");
        }
    }

    // TEMPORARY: old vs new, same binary, back to back.
    #[test]
    fn tmp_time_convert() {
        let (w, h) = (std::hint::black_box(1920usize), 1080usize);
        let mut seed = 1u64;
        let host: Vec<u8> = (0..w * h * 4)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (seed >> 56) as u8
            })
            .collect();
        let mut e = Encoder { child: Command::new("true").spawn().unwrap(), stdin: None, units: channel().1, pending: VecDeque::new(), yuv: vec![0; w * h * 3 / 2], w, h };
        let mut yuv = vec![0u8; w * h * 3 / 2];
        let mut old = |host: &[u8]| {
            let (ys, uv) = yuv.split_at_mut(w * h);
            let (us, vs) = uv.split_at_mut(w * h / 4);
            convert_ref(host, w * 4, w, ys, us, vs);
        };
        old(&host);
        e.upload(&host, w * 4, (0, h as i32));
        let (mut ta, mut tb) = (vec![], vec![]);
        for _ in 0..10 {
            let t = std::time::Instant::now();
            for _ in 0..20 {
                old(std::hint::black_box(&host));
            }
            ta.push(t.elapsed().as_secs_f64() * 1e3 / 20.0);
            let t = std::time::Instant::now();
            for _ in 0..20 {
                e.upload(std::hint::black_box(&host), w * 4, (0, h as i32));
            }
            tb.push(t.elapsed().as_secs_f64() * 1e3 / 20.0);
        }
        ta.sort_by(f64::total_cmp);
        tb.sort_by(f64::total_cmp);
        println!("TIMING old med {:.3} best {:.3} | new med {:.3} best {:.3} ms/upload", ta[5], ta[0], tb[5], tb[0]);
    }

    #[test]
    fn splits_access_units() {
        let s = [0, 0, 0, 1, 9, 0xF0, 0, 0, 1, 0x65, 7, 0, 0, 0, 1, 9, 0xF0, 0, 0, 1, 0x41, 8];
        assert_eq!(next_unit(&s), Some(11)); // the second delimiter, with its 4-byte start code
        assert_eq!(next_unit(&s[11..]), None);
    }
}
