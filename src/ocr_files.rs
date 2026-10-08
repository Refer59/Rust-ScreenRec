//! The text recognizer's files, pinned by URL and SHA-256: ONNX Runtime for this OS and CPU,
//! and PaddleOCR's PP-OCRv6 small detector and recognizer (official ONNX exports, Apache-2.0).
//! `screenrec install` downloads them into `desktop::data_dir()/ocr`; nothing else ever
//! downloads: a missing file at recognition time is an error that says to run it.

use crate::{Res, desktop, service};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// One file we install: `name` in the ocr folder, from `url`, hashed `sha256`; a runtime
/// comes as an archive (hashed `archive_sha256`) whose `member` is the library.
struct Asset {
    name: &'static str,
    url: &'static str,
    sha256: &'static str,
    archive: Option<(&'static str, &'static str)>, // (member as the archive lists it, the archive's sha256)
    mb: f32,
}

const DET: Asset = Asset { name: "det.onnx", url: "https://huggingface.co/PaddlePaddle/PP-OCRv6_small_det_onnx/resolve/main/inference.onnx", sha256: "d73e0058b7a8086bbd57f3d10b8bcd4ff95363f67e06e2762b5e814fe9c9410e", archive: None, mb: 9.9 };
const REC: Asset = Asset { name: "rec.onnx", url: "https://huggingface.co/PaddlePaddle/PP-OCRv6_small_rec_onnx/resolve/main/inference.onnx", sha256: "5435fd747c9e0efe15a96d0b378d5bd157e9492ed8fd80edf08f30d02fa24634", archive: None, mb: 21.2 };
/// The recognizer's config: its character dictionary (18 708 entries) lives in it.
const DICT: Asset = Asset { name: "rec.yml", url: "https://huggingface.co/PaddlePaddle/PP-OCRv6_small_rec_onnx/resolve/main/inference.yml", sha256: "ab078671bb49f06228eadccd34f1bb501e157f7a047095ffb943ba81512c77d1", archive: None, mb: 0.2 };

/// ONNX Runtime 1.30.0, except on Intel Macs, whose last release is 1.23.2 (the API we ask
/// for, 23, is the same). Only the library is unpacked: the Windows zip is mostly a .pdb.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const RUNTIME: Option<Asset> = Some(Asset { name: "libonnxruntime.so", url: "https://github.com/microsoft/onnxruntime/releases/download/v1.30.0/onnxruntime-linux-x64-1.30.0.tgz", sha256: "245a6f8c38127551057a1cd1ffd59f0a186a227ade4f3492dea2494eb565542e", archive: Some(("onnxruntime-linux-x64-1.30.0/lib/libonnxruntime.so.1.30.0", "a5ed5a3cac51fbb2e90da632ae43d19212faaa20e76484e62bcb7c23ddb3b3fd")), mb: 11.3 });
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const RUNTIME: Option<Asset> = Some(Asset { name: "libonnxruntime.so", url: "https://github.com/microsoft/onnxruntime/releases/download/v1.30.0/onnxruntime-linux-aarch64-1.30.0.tgz", sha256: "64e903a43a041240fd6bcffe0ac6d4fea47ef87bf24b9d097801bd00a9612a4b", archive: Some(("onnxruntime-linux-aarch64-1.30.0/lib/libonnxruntime.so.1.30.0", "e16a27a8ed330bbc698df7330b0cf56e722f354e3bcc92118682c74ef3c3e3da")), mb: 10.3 });
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const RUNTIME: Option<Asset> = Some(Asset { name: "libonnxruntime.dylib", url: "https://github.com/microsoft/onnxruntime/releases/download/v1.30.0/onnxruntime-osx-arm64-1.30.0.tgz", sha256: "bcc9110f9d638a119de2db7afb3ba9a1da8085f0cb3401e1c48ae1caf450b6fa", archive: Some(("./onnxruntime-osx-arm64-1.30.0/lib/libonnxruntime.1.30.0.dylib", "6ebb5062a934537c352937821f9fe9718e7de1a2db1122a93dd363ffd53a7012")), mb: 42.4 });
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const RUNTIME: Option<Asset> = Some(Asset { name: "libonnxruntime.dylib", url: "https://github.com/microsoft/onnxruntime/releases/download/v1.23.2/onnxruntime-osx-x86_64-1.23.2.tgz", sha256: "8c9c78de65ea3786f987c0d980e9c1b13a3a5fbc6b3e2965ba05b450e6e4c054", archive: Some(("./onnxruntime-osx-x86_64-1.23.2/lib/libonnxruntime.1.23.2.dylib", "d10359e16347b57d9959f7e80a225a5b4a66ed7d7e007274a15cae86836485a6")), mb: 11.7 });
#[cfg(all(windows, target_arch = "x86_64"))]
const RUNTIME: Option<Asset> = Some(Asset { name: "onnxruntime.dll", url: "https://github.com/microsoft/onnxruntime/releases/download/v1.30.0/onnxruntime-win-x64-1.30.0.zip", sha256: "7e39e2bdbba836d98071ef28620735ba36a47c554cf794585269aecc50fab0da", archive: Some(("onnxruntime-win-x64-1.30.0/lib/onnxruntime.dll", "c6ba983baf5681af108599675d2a89c2d145512d02de28aed0bff177cd0ba949")), mb: 82.6 });
#[cfg(not(any(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")), all(target_os = "macos", any(target_arch = "x86_64", target_arch = "aarch64")), all(windows, target_arch = "x86_64"))))]
const RUNTIME: Option<Asset> = None;

/// The installed files, all present.
pub struct Files {
    pub runtime: PathBuf,
    pub det: PathBuf,
    pub rec: PathBuf,
    pub dict: PathBuf,
}

/// Where they live.
pub fn dir() -> PathBuf {
    desktop::data_dir().join("ocr")
}

fn no_runtime() -> String {
    tr!("text recognition isn't available for this OS and CPU", "el reconocimiento de texto no está disponible para este sistema y CPU", "この OS と CPU ではテキスト認識を利用できません")
}

/// The installed files; an error naming what to do if any is missing.
pub fn files() -> Res<Files> {
    let dir = dir();
    let Some(rt) = RUNTIME else { return Err(no_runtime().into()) };
    let f = Files { runtime: dir.join(rt.name), det: dir.join(DET.name), rec: dir.join(REC.name), dict: dir.join(DICT.name) };
    if [&f.runtime, &f.det, &f.rec, &f.dict].iter().all(|p| p.is_file()) {
        return Ok(f);
    }
    Err(tr!("text recognition is not installed: run `screenrec install`", "el reconocimiento de texto no está instalado: ejecuta `screenrec install`", "テキスト認識がインストールされていません: `screenrec install` を実行してください").into())
}

/// The `screenrec install` step: download what is missing or damaged (each file is
/// verified by its SHA-256), unpack the runtime, and say what was done on `out`.
pub fn install(out: &mut impl Write) -> Res<()> {
    let Some(rt) = RUNTIME else {
        writeln!(out, "{}", no_runtime())?;
        return Ok(());
    };
    let dir = dir();
    std::fs::create_dir_all(&dir)?;
    let mut fetched = 0;
    for a in [&rt, &DET, &REC, &DICT] {
        let dest = dir.join(a.name);
        if dest.is_file() && sha256_file(&dest)? == a.sha256 {
            continue;
        }
        writeln!(out, "{}", tr!("downloading {} ({:.0} MB)...", "descargando {} ({:.0} MB)...", "{} をダウンロード中 ({:.0} MB)...", a.url, a.mb))?;
        out.flush()?;
        let _ = std::fs::remove_file(&dest);
        match a.archive {
            None => {
                service::download(a.url, &dest)?;
                check(&dest, a.sha256)?;
            }
            Some((member, archive_sha256)) => {
                let archive = dir.join(a.url.rsplit('/').next().unwrap_or("archive"));
                let unpacked = dir.join("unpack");
                let res = (|| -> Res<()> {
                    service::download(a.url, &archive)?;
                    check(&archive, archive_sha256)?;
                    service::unpack(&archive, &unpacked, &[member])?;
                    std::fs::rename(unpacked.join(member), &dest)?;
                    check(&dest, a.sha256)
                })();
                let _ = std::fs::remove_file(&archive);
                let _ = std::fs::remove_dir_all(&unpacked);
                res?;
            }
        }
        fetched += 1;
    }
    writeln!(
        out,
        "{}",
        match fetched {
            0 => tr!("text recognition: ready in {}", "reconocimiento de texto: listo en {}", "テキスト認識: {} に準備済み", dir.display()),
            _ => tr!("text recognition: installed in {}", "reconocimiento de texto: instalado en {}", "テキスト認識: {} にインストールしました", dir.display()),
        }
    )?;
    Ok(())
}

/// Error unless the installed runtime is the pinned one. A damaged library would crash the
/// loader rather than fail (a truncated one maps pages past its end: SIGBUS), so the service
/// hashes it before loading it, once per start (~0.1 s for 29 MB). A wrong one is removed, so
/// the next request says to install again.
pub fn verify_runtime(path: &Path) -> Res<()> {
    let Some(rt) = RUNTIME else { return Err(no_runtime().into()) };
    check(path, rt.sha256).map_err(|e| tr!("{}: run `screenrec install`", "{}: ejecuta `screenrec install`", "{}: `screenrec install` を実行してください", e).into())
}

/// Error unless `path` hashes to `sha256`; a wrong file is removed.
fn check(path: &Path, sha256: &str) -> Res<()> {
    if sha256_file(path)? == sha256 {
        return Ok(());
    }
    let _ = std::fs::remove_file(path);
    Err(tr!("{} is not the file expected (checksum mismatch)", "{} no es el archivo esperado (la suma no coincide)", "{} は想定したファイルではありません (チェックサム不一致)", path.display()).into())
}

/// The recognizer's characters, from its config: the `character_dict` list, one per line
/// as `  - X` or `  - 'X'` (a '' inside quotes is one quote). Index i of the model's
/// output is entry i-1; 0 is CTC's blank and one past the end is a space.
pub fn dict(yml: &str) -> Res<Vec<String>> {
    let mut out = Vec::with_capacity(20_000);
    let mut inside = false;
    for line in yml.lines() {
        if !inside {
            inside = line.trim_end() == "  character_dict:";
            continue;
        }
        let Some(item) = line.strip_prefix("  - ") else { break };
        out.push(match item.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
            Some(q) => q.replace("''", "'"),
            None => item.to_owned(),
        });
    }
    if out.len() < 1000 {
        return Err(tr!("the recognizer's dictionary is damaged: run `screenrec install`", "el diccionario del reconocedor está dañado: ejecuta `screenrec install`", "認識器の辞書が壊れています: `screenrec install` を実行してください").into());
    }
    Ok(out)
}

/// Hex SHA-256 of the file at `path`.
pub fn sha256_file(path: &Path) -> Res<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finish().iter().map(|b| format!("{b:02x}")).collect())
}

/// SHA-256 (FIPS 180-4), streaming. Fifty lines beat a dependency for four files.
pub struct Sha256 {
    h: [u32; 8],
    tail: Vec<u8>,
    len: u64,
}

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
    0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
    0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
    0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub fn new() -> Self {
        Sha256 { h: [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19], tail: Vec::with_capacity(128), len: 0 }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.len += data.len() as u64;
        if !self.tail.is_empty() {
            let take = (64 - self.tail.len()).min(data.len());
            self.tail.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.tail.len() < 64 {
                return;
            }
            let block: [u8; 64] = self.tail[..].try_into().unwrap();
            self.block(&block);
            self.tail.clear();
        }
        let (blocks, rest) = data.as_chunks::<64>();
        for b in blocks {
            self.block(b);
        }
        self.tail.extend_from_slice(rest);
    }

    pub fn finish(mut self) -> [u8; 32] {
        let bits = self.len * 8;
        let mut pad = vec![0x80u8];
        pad.resize(1 + (119 - self.tail.len() % 64) % 64, 0);
        pad.extend_from_slice(&bits.to_be_bytes());
        self.len = 0; // the padding doesn't count (already folded into `bits`)
        self.update(&pad);
        debug_assert!(self.tail.is_empty());
        let mut out = [0u8; 32];
        for (o, w) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.h) {
            *o = w.to_be_bytes();
        }
        out
    }

    fn block(&mut self, b: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (i, c) in b.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*c);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut bb, mut c, mut d, mut e, mut f, mut g, mut h] = self.h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & bb) ^ (a & c) ^ (bb & c);
            let t2 = s0.wrapping_add(maj);
            (h, g, f, e, d, c, bb, a) = (g, f, e, d.wrapping_add(t1), c, bb, a, t1.wrapping_add(t2));
        }
        for (hi, v) in self.h.iter_mut().zip([a, bb, c, d, e, f, g, h]) {
            *hi = hi.wrapping_add(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(d: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(d);
        h.finish().iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn sha256_matches_the_standard_vectors() {
        assert_eq!(hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"), "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
        let million = vec![b'a'; 1_000_000];
        assert_eq!(hex(&million), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
        // Streaming in odd pieces gives the same digest.
        let mut h = Sha256::new();
        for piece in million.chunks(777) {
            h.update(piece);
        }
        assert_eq!(h.finish().iter().map(|b| format!("{b:02x}")).collect::<String>(), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
    }

    #[test]
    fn dictionary_comes_out_of_the_config() {
        let mut yml = String::from("Global:\n  model_name: x\nPostProcess:\n  character_dict:\n  - '!'\n  - $\n  - ''''\n  - ñ\n  - 日\n  - '#'\n");
        for i in 0..1000 {
            yml.push_str(&format!("  - x{i}\n"));
        }
        yml.push_str("  name: CTCLabelDecode\n");
        let d = dict(&yml).unwrap();
        assert_eq!(&d[..6], ["!", "$", "'", "ñ", "日", "#"]);
        assert_eq!(d.len(), 1006);
        assert!(dict("nothing: here\n").is_err());
    }

    /// Needs the network: the real files into a private data folder (set XDG_DATA_HOME /
    /// LOCALAPPDATA / HOME to keep it off the user's). CI runs it; `cargo test` skips it.
    #[test]
    #[ignore]
    fn ocr_install_fetches_and_verifies_the_files() {
        let mut out = Vec::new();
        install(&mut out).unwrap();
        let f = files().unwrap();
        for p in [&f.runtime, &f.det, &f.rec, &f.dict] {
            assert!(p.is_file(), "{}", p.display());
        }
        assert_eq!(sha256_file(&f.det).unwrap(), DET.sha256);
        assert_eq!(dict(&std::fs::read_to_string(&f.dict).unwrap()).unwrap().len(), 18_708);
        // A second run finds everything in place and downloads nothing.
        let mut again = Vec::new();
        install(&mut again).unwrap();
        assert!(!String::from_utf8_lossy(&again).contains("http"), "{}", String::from_utf8_lossy(&again));
    }
}
