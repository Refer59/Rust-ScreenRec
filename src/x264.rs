//! H.264 without a GPU: x264 (ultrafast, zerolatency) in an ffmpeg process.
//! It gets I420 converted here, and only for the rows that changed: ffmpeg's
//! own BGRX->YUV conversion costs as much as the encoding itself.

use crate::Res;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};

pub struct Encoder {
    child: Child,
    stdin: Option<ChildStdin>,
    units: Receiver<Vec<u8>>, // access units (Annex B), one per frame, in order
    pending: VecDeque<u64>,   // stamps of the frames sent and not back yet
    yuv: Vec<u8>,
    w: usize,
    h: usize,
}

/// Where the access unit after the first one starts (x264's aud=1 opens each
/// with a delimiter NAL; NAL payloads never contain 00 00 01).
fn next_unit(b: &[u8]) -> Option<usize> {
    (4..b.len().saturating_sub(3)).find(|&i| b[i..i + 4] == [0, 0, 1, 9]).map(|i| if b[i - 1] == 0 { i - 1 } else { i })
}

impl Encoder {
    pub fn new(w: usize, h: usize, fps: u32) -> Res<Self> {
        let (size, rate) = (format!("{w}x{h}"), fps.to_string());
        #[rustfmt::skip]
        let args = [
            "-v", "error", "-f", "rawvideo", "-pix_fmt", "yuv420p", "-s", &size, "-r", &rate, "-i", "-",
            "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency", "-crf", "23", "-g", "300", "-x264-params", "aud=1",
            "-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-color_range", "tv",
            "-vsync", "0", "-flush_packets", "1", "-f", "h264", "-",
        ];
        let mut child = Command::new("ffmpeg")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "grabar sin GPU necesita ffmpeg (sudo apt install ffmpeg)")?;
        let (stdin, mut out) = (child.stdin.take(), child.stdout.take().ok_or("ffmpeg sin stdout")?);
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
        let stdin = self.stdin.as_mut().ok_or("ffmpeg ya terminó")?;
        stdin.write_all(&self.yuv).map_err(|_| "ffmpeg dejó de codificar (¿tiene libx264?)")?;
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
/// `vs`, which cover the same row pairs. Pixels are read as u32 and the
/// chroma math fits i16, so it vectorizes with plain SSE2 (~3 ms per 1080p).
fn convert(src: &[u8], stride: usize, w: usize, ys: &mut [u8], us: &mut [u8], vs: &mut [u8]) {
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
        let mut e = Encoder { child: Command::new("true").spawn().unwrap(), stdin: None, units: channel().1, pending: VecDeque::new(), yuv: vec![0; 6], w: 2, h: 2 };
        e.upload(&[0, 0, 255, 0].repeat(4), 8, (0, 2)); // pure red
        assert_eq!(e.yuv, [63, 63, 63, 63, 102, 240]);
        e.upload(&[255; 16], 8, (0, 2));
        assert_eq!(e.yuv, [235, 235, 235, 235, 128, 128]);
    }

    #[test]
    fn splits_access_units() {
        let s = [0, 0, 0, 1, 9, 0xF0, 0, 0, 1, 0x65, 7, 0, 0, 0, 1, 9, 0xF0, 0, 0, 1, 0x41, 8];
        assert_eq!(next_unit(&s), Some(11)); // the second delimiter, with its 4-byte start code
        assert_eq!(next_unit(&s[11..]), None);
    }
}
