//! Streaming Matroska writer: an H.264 track, plus an Opus one if there is
//! sound. Each block is one unbuffered
//! write, so a crash loses nothing already encoded; sizes, Duration, Cues and
//! SeekHead are patched in `finish`, and without them the file still plays
//! (unknown-size elements are legal).

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

const UNKNOWN_SIZE: [u8; 8] = [0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
const SEEK_SPACE: usize = 64; // reserved up front for the SeekHead that points at Cues
const DUR_SPACE: usize = 11; // Duration element: 2 id + 1 size + 8 f64

fn id(id: u32) -> Vec<u8> {
    id.to_be_bytes().into_iter().skip_while(|&b| b == 0).collect()
}

/// Shortest EBML size vint (all-ones values are reserved for "unknown").
fn size(n: usize) -> Vec<u8> {
    let n = n as u64;
    let len = (1..=8).find(|&l| n < (1 << (7 * l)) - 1).unwrap();
    (n | 1 << (7 * len)).to_be_bytes()[8 - len..].to_vec()
}

/// 8-byte size field, patchable in place over UNKNOWN_SIZE.
fn size8(n: u64) -> [u8; 8] {
    (n | 1 << 56).to_be_bytes()
}

fn el(i: u32, data: &[u8]) -> Vec<u8> {
    [id(i), size(data.len()), data.to_vec()].concat()
}

fn uint(i: u32, v: u64) -> Vec<u8> {
    el(i, &v.to_be_bytes()[(v.leading_zeros() as usize / 8).min(7)..])
}

fn void(len: usize) -> Vec<u8> {
    let mut v = vec![0; len];
    (v[0], v[1]) = (0xEC, 0x80 | (len - 2) as u8);
    v
}

/// Index of the first zero byte, 8 bytes at a time.
fn zero(b: &[u8]) -> Option<usize> {
    const LO: u64 = u64::from_ne_bytes([0x01; 8]);
    const HI: u64 = u64::from_ne_bytes([0x80; 8]);
    let (chunks, tail) = b.as_chunks::<8>();
    for (k, c) in chunks.iter().enumerate() {
        let v = u64::from_ne_bytes(*c);
        if v.wrapping_sub(LO) & !v & HI != 0 {
            return c.iter().position(|&x| x == 0).map(|p| k * 8 + p);
        }
    }
    tail.iter().position(|&x| x == 0).map(|p| chunks.len() * 8 + p)
}

/// Index of the first `00 00 01` at or after `i`.
fn start_code(b: &[u8], mut i: usize) -> Option<usize> {
    while i + 3 <= b.len() {
        let z = i + zero(&b[i..b.len() - 2])?;
        if b[z + 1] == 0 && b[z + 2] == 1 {
            return Some(z);
        }
        i = z + 1;
    }
    None
}

/// NAL units of an Annex B stream.
fn nals(b: &[u8]) -> Vec<&[u8]> {
    let mut starts = vec![];
    let mut i = 0;
    while let Some(z) = start_code(b, i) {
        starts.push(z + 3);
        i = z + 3;
    }
    let ends = starts.iter().skip(1).map(|&s| s - 3).chain([b.len()]);
    starts
        .iter()
        .zip(ends)
        .map(|(&s, e)| {
            let mut nal = &b[s..e];
            while let [rest @ .., 0] = nal {
                nal = rest; // zero_byte of the next 4-byte start code
            }
            nal
        })
        .filter(|n| !n.is_empty())
        .collect()
}

/// Whether an encoded frame (Annex B) is a keyframe (has an IDR slice).
pub fn is_keyframe(annexb: &[u8]) -> bool {
    nals(annexb).iter().any(|n| n[0] & 0x1F == 5)
}

pub struct Mkv {
    f: File,
    pos: u64,
    seg: u64, // start of the Segment payload; SeekHead/Cues positions are relative to it
    dur_at: u64,
    cluster: Option<(u64, u64)>, // (offset of its size field, timestamp)
    cues: Vec<u8>,
}

impl Mkv {
    /// `first` is the first encoded frame (Annex B); its SPS/PPS become the
    /// codec header. `opus` is the audio track's (OpusHead, encoder delay in samples).
    pub fn create(path: &Path, w: usize, h: usize, first: &[u8], opus: Option<(Vec<u8>, u16)>) -> io::Result<Self> {
        let n = nals(first);
        let find = |t| n.iter().find(|nal| nal[0] & 0x1F == t).copied();
        let (Some(sps), Some(pps)) = (find(7), find(8)) else {
            return Err(io::Error::other(tr!("the first frame has no SPS/PPS", "el primer frame no trae SPS/PPS", "最初のフレームに SPS/PPS がありません")));
        };
        let mut avcc = vec![1, sps[1], sps[2], sps[3], 0xFF, 0xE1];
        avcc.extend((sps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(sps);
        avcc.push(1);
        avcc.extend((pps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(pps);

        let mut m = Mkv { f: File::create(path)?, pos: 0, seg: 0, dur_at: 0, cluster: None, cues: vec![] };
        let ebml = [uint(0x4286, 1), uint(0x42F7, 1), uint(0x42F2, 4), uint(0x42F3, 8), el(0x4282, b"matroska"), uint(0x4287, 4), uint(0x4285, 2)];
        m.put(&el(0x1A45DFA3, &ebml.concat()))?;
        m.put(&[id(0x18538067), UNKNOWN_SIZE.to_vec()].concat())?; // Segment
        m.seg = m.pos;
        m.put(&void(SEEK_SPACE))?;
        let info = [uint(0x2AD7B1, 1_000_000), el(0x4D80, b"screenrec"), el(0x5741, b"screenrec"), void(DUR_SPACE)];
        let info = el(0x1549A966, &info.concat());
        m.dur_at = m.pos + (info.len() - DUR_SPACE) as u64;
        m.put(&info)?;
        let video = el(0xE0, &[uint(0xB0, w as u64), uint(0xBA, h as u64)].concat());
        let track = [uint(0xD7, 1), uint(0x73C5, 1), uint(0x83, 1), uint(0x9C, 0), el(0x86, b"V_MPEG4/ISO/AVC"), el(0x63A2, &avcc), video];
        let mut tracks = el(0xAE, &track.concat());
        if let Some((head, delay)) = opus {
            let audio = el(0xE1, &[el(0xB5, &48000f64.to_be_bytes()), uint(0x9F, 2)].concat());
            let delay_ns = delay as u64 * 1_000_000_000 / 48000;
            let codec = [el(0x86, b"A_OPUS"), el(0x63A2, &head), uint(0x56AA, delay_ns), uint(0x56BB, 80_000_000)];
            let track = [uint(0xD7, 2), uint(0x73C5, 2), uint(0x83, 2), codec.concat(), audio];
            tracks.extend(el(0xAE, &track.concat()));
        }
        m.put(&el(0x1654AE6B, &tracks))?;
        Ok(m)
    }

    fn put(&mut self, b: &[u8]) -> io::Result<()> {
        self.pos += b.len() as u64;
        self.f.write_all(b)
    }

    /// Overwrite bytes at `at`, then carry on appending at the end.
    fn patch(&self, at: u64, b: &[u8]) -> io::Result<()> {
        let mut f = &self.f;
        f.seek(SeekFrom::Start(at))?;
        f.write_all(b)?;
        f.seek(SeekFrom::Start(self.pos)).map(drop)
    }

    /// Append one encoded video frame (Annex B), `ts` in ms.
    pub fn frame(&mut self, ts: u64, key: bool, annexb: &[u8]) -> io::Result<()> {
        // SPS/PPS/AUD live in CodecPrivate; the rest goes length-prefixed.
        let n: Vec<_> = nals(annexb).into_iter().filter(|n| !matches!(n[0] & 0x1F, 7..=9)).collect();
        let len = n.iter().map(|nal| 4 + nal.len()).sum();
        self.block(1, ts, key, len, |out| {
            for nal in n {
                out.extend((nal.len() as u32).to_be_bytes());
                out.extend_from_slice(nal);
            }
        })
    }

    /// Append one Opus packet, `ts` in ms.
    pub fn audio(&mut self, ts: u64, packet: &[u8]) -> io::Result<()> {
        self.block(2, ts, false, packet.len(), |out| out.extend_from_slice(packet))
    }

    /// Video keyframes start a cluster (and get a cue); so does a block too
    /// far from the cluster's timestamp for the 16-bit relative one. Audio
    /// trails video by ~0.1 s, so relative timestamps can be negative.
    /// `fill` appends the `len` payload bytes.
    fn block(&mut self, track: u8, ts: u64, key: bool, len: usize, fill: impl FnOnce(&mut Vec<u8>)) -> io::Result<()> {
        let mut out = vec![];
        if key || self.cluster.is_none_or(|(_, t)| (ts as i64 - t as i64).abs() > 30_000) {
            self.close_cluster()?;
            if key {
                let pos = [uint(0xF7, 1), uint(0xF1, self.pos - self.seg)].concat();
                self.cues.extend(el(0xBB, &[uint(0xB3, ts), el(0xB7, &pos)].concat()));
            }
            self.cluster = Some((self.pos + 4, ts));
            out = [id(0x1F43B675), UNKNOWN_SIZE.to_vec(), uint(0xE7, ts)].concat();
        }
        let rel = (ts as i64 - self.cluster.unwrap().1 as i64) as i16;
        let flags = if key || track != 1 { 0x80 } else { 0 }; // every Opus packet decodes on its own
        out.reserve(1 + 8 + 4 + len);
        out.extend(id(0xA3));
        out.extend(size(4 + len));
        out.extend([0x80 | track, (rel >> 8) as u8, rel as u8, flags]);
        fill(&mut out);
        self.put(&out)
    }

    fn close_cluster(&mut self) -> io::Result<()> {
        if let Some((at, _)) = self.cluster.take() {
            self.patch(at, &size8(self.pos - at - 8))?;
        }
        Ok(())
    }

    pub fn finish(mut self, duration_ms: u64) -> io::Result<()> {
        self.close_cluster()?;
        let cues_at = self.pos - self.seg;
        let cues = std::mem::take(&mut self.cues);
        self.put(&el(0x1C53BB6B, &cues))?;
        let seek = el(0x114D9B74, &el(0x4DBB, &[el(0x53AB, &id(0x1C53BB6B)), uint(0x53AC, cues_at)].concat()));
        let pad = void(SEEK_SPACE - seek.len());
        self.patch(self.seg, &[seek, pad].concat())?;
        self.patch(self.dur_at, &el(0x4489, &(duration_ms as f64).to_be_bytes()))?;
        self.patch(self.seg - 8, &size8(self.pos - self.seg))?;
        self.f.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ebml_and_annexb() {
        assert_eq!(size(0), [0x80]);
        assert_eq!(size(126), [0xFE]);
        assert_eq!(size(127), [0x40, 0x7F]); // 0xFF would mean "unknown"
        assert_eq!(uint(0xD7, 0), [0xD7, 0x81, 0]);
        assert_eq!(uint(0xB0, 1920), [0xB0, 0x82, 0x07, 0x80]);
        assert_eq!(void(11).len(), 11);
        let s = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 0, 0];
        assert_eq!(nals(&s), [&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 4]]);
    }

    /// The original byte-by-byte scan, kept as the reference.
    fn nals_ref(b: &[u8]) -> Vec<&[u8]> {
        let mut starts = vec![];
        let mut i = 0;
        while i + 3 <= b.len() {
            if b[i..i + 3] == [0, 0, 1] {
                starts.push(i + 3);
                i += 3;
            } else {
                i += 1;
            }
        }
        let ends = starts.iter().skip(1).map(|&s| s - 3).chain([b.len()]);
        starts
            .iter()
            .zip(ends)
            .map(|(&s, e)| {
                let mut nal = &b[s..e];
                while let [rest @ .., 0] = nal {
                    nal = rest;
                }
                nal
            })
            .filter(|n| !n.is_empty())
            .collect()
    }

    #[test]
    fn nals_match_reference() {
        let mut cases: Vec<Vec<u8>> = vec![
            vec![],
            vec![0],
            vec![0, 0],
            vec![0, 0, 1],
            vec![0, 0, 0, 1],
            vec![0, 0, 1, 0x65, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
            vec![0, 0, 0, 1, 0x67, 1, 0, 0, 0, 0, 0, 1, 0x68, 2, 0, 0, 1, 0x65, 3, 0, 0, 0],
            vec![0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 1, 0, 0, 1],
            vec![0x41, 0, 7, 0, 0, 2, 9, 0, 0, 3, 0, 0, 1, 0x41, 0, 0, 3, 1, 0, 0, 2, 0, 5, 0],
            vec![9, 9, 9, 0, 0, 1, 0x65, 0, 1, 0, 0, 0, 0, 1, 0x41],
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 0, 0],
        ];
        // every length 0..=40 of a stream with start codes near chunk edges
        let base = [0, 0, 0, 1, 0x67, 5, 6, 7, 0, 0, 1, 0x68, 0, 0, 2, 0, 0, 0, 1, 0x65, 1, 2, 3, 4, 5, 6, 7, 0, 0, 1, 0x41, 0, 0, 3, 8, 9, 0, 0, 1, 0x01, 0];
        cases.extend((0..=base.len()).map(|n| base[..n].to_vec()));
        // pseudo-random 20 KB, mostly nonzero, with zeros and start codes sprinkled in
        let mut x = 12345u32;
        let mut rnd = || {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            x >> 16
        };
        let mut r = vec![];
        while r.len() < 20_000 {
            match rnd() % 64 {
                0 => r.extend([0, 0, 1]),
                1 => r.extend([0, 0, 0, 1]),
                2 => r.extend([0, 0, rnd() as u8 % 4]),
                3..=6 => r.push(0),
                _ => r.push(rnd() as u8),
            }
        }
        cases.push(r);
        for c in &cases {
            assert_eq!(nals(c), nals_ref(c), "{c:?}");
        }
    }
}
