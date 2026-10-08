//! Text recognition: PaddleOCR's PP-OCRv6 small detector and recognizer (one model for English,
//! Spanish and 45 other Latin-script languages, Japanese and Chinese), run by ONNX Runtime, which
//! is loaded at run time from the files `screenrec install` put in the data folder (ocr_files.rs).
//! The same pipeline as PaddleOCR's own: the DB detector at "shortest side at least 736" (a
//! 1080p screen stays full size), text boxes from its probability map, each line cut out, read
//! at 48 px high and CTC-decoded against the model's dictionary.

use crate::ocr_files::{self, Files};
use crate::ort_sys::*;
use crate::{Res, dylib};
use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr::null_mut;

/// A line of text: where it is (pixels of the image given) and what it says, with the
/// recognizer's mean confidence (0..1).
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub text: String,
    pub conf: f32,
}

/// Lines in reading order, one per line of text.
pub fn text_of(lines: &[Line]) -> String {
    let mut s = String::new();
    for l in lines {
        s.push_str(&l.text);
        s.push('\n');
    }
    s
}

// Detector (DBNet) post-processing, PaddleOCR's defaults.
const DET_MIN_SIDE: f32 = 736.0; // the shortest side is scaled up to this; never scaled down
const DET_MAX_SIDE: f32 = 1920.0; // ... unless that would take the longest side past this (a thin strip)
const DET_THRESH: f32 = 0.3; // probability map -> text pixels
const BOX_THRESH: f32 = 0.5; // mean probability a box needs
const UNCLIP: f32 = 1.6; // boxes grow by area * this / perimeter: the map is of shrunken text
const REC_H: usize = 48; // the recognizer reads lines this high ...
const REC_MIN_W: usize = 320; // ... and at least this wide (the rest padded), as it was trained
const TEXT_THRESH: f32 = 0.5; // lines read with less confidence are dropped
const THREADS: i32 = 2; // ORT's intra-op threads: the service shares the machine with the user

/// ONNX Runtime's API table, checked for errors.
struct Api(&'static OrtApi);

impl Api {
    fn ok(&self, st: *mut OrtStatus, what: &str) -> Res<()> {
        if st.is_null() {
            return Ok(());
        }
        let msg = unsafe { CStr::from_ptr(self.0.GetErrorMessage.unwrap()(st)) }.to_string_lossy().into_owned();
        unsafe { self.0.ReleaseStatus.unwrap()(st) };
        Err(format!("{what}: {msg}").into())
    }
}

/// `ort!(api, Function(args...))`: call it and turn its status into a Result.
macro_rules! ort {
    ($api:expr, $f:ident($($a:expr),*)) => { $api.ok(unsafe { $api.0.$f.unwrap()($($a),*) }, stringify!($f)) };
}

struct Session {
    sess: *mut OrtSession,
    input: CString,
    output: CString,
}

/// A loaded recognizer: runtime, both models and the dictionary. Lives on the thread that
/// made it (the service's worker).
pub struct Engine {
    api: Api,
    env: *mut OrtEnv,
    det: Session,
    rec: Session,
    dict: Vec<String>,
}

/// Why a library didn't load, in the OS's words (a too-old glibc says which symbol).
fn load_error() -> String {
    #[cfg(unix)]
    {
        let p = unsafe { libc::dlerror() };
        if !p.is_null() {
            return unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        }
    }
    let e = std::io::Error::last_os_error();
    // "Module not found" although the DLL is there: a DLL it needs is missing, which for
    // ONNX Runtime is the Visual C++ runtime (MSVCP140.dll, VCRUNTIME140.dll).
    #[cfg(windows)]
    {
        if e.raw_os_error() == Some(126) {
            let hint = tr!(
                "a library it needs is missing: install the Microsoft Visual C++ Redistributable, https://aka.ms/vs/17/release/vc_redist.x64.exe",
                "falta una biblioteca que necesita: instala el Microsoft Visual C++ Redistributable, https://aka.ms/vs/17/release/vc_redist.x64.exe",
                "必要なライブラリがありません: Microsoft Visual C++ 再頒布可能パッケージをインストールしてください (https://aka.ms/vs/17/release/vc_redist.x64.exe)"
            );
            return format!("{e}; {hint}");
        }
    }
    e.to_string()
}

fn reinstall() -> String {
    tr!("run `screenrec install` to repair it", "ejecuta `screenrec install` para repararlo", "`screenrec install` を実行して修復してください")
}

impl Engine {
    /// Load the runtime and both models from `files` (ocr_files::files()): ~0.3 s and ~90 MB.
    pub fn load(files: &Files) -> Res<Engine> {
        ocr_files::verify_runtime(&files.runtime)?;
        let lib = dylib::open_path(&files.runtime).ok_or_else(|| {
            let why = load_error(); // first: on Windows, tr!'s own calls would overwrite the error
            tr!("couldn't load {}: {}", "no se pudo cargar {}: {}", "{} を読み込めませんでした: {}", files.runtime.display(), why)
        })?;
        let base: unsafe extern "C" fn() -> *const OrtApiBase = unsafe { dylib::sym(lib, c"OrtGetApiBase") }.ok_or_else(|| tr!("{} is not ONNX Runtime", "{} no es ONNX Runtime", "{} は ONNX Runtime ではありません", files.runtime.display()))?;
        let base = unsafe { &*base() };
        let version = unsafe { CStr::from_ptr(base.GetVersionString.unwrap()()) }.to_string_lossy().into_owned();
        let api = unsafe { base.GetApi.unwrap()(ORT_API_VERSION) };
        if api.is_null() {
            return Err(tr!("ONNX Runtime {} is too old (API {} needed): {}", "ONNX Runtime {} es demasiado antiguo (hace falta la API {}): {}", "ONNX Runtime {} は古すぎます (API {} が必要): {}", version, ORT_API_VERSION, reinstall()).into());
        }
        let api = Api(unsafe { &*api });
        let mut env: *mut OrtEnv = null_mut();
        ort!(api, CreateEnv(ORT_LOGGING_LEVEL_ERROR, c"screenrec".as_ptr(), &mut env))?;
        let model = |path: &std::path::Path| -> Res<Session> {
            let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            session(&api, env, &bytes).map_err(|e| format!("{}: {e} ({})", path.display(), reinstall()).into())
        };
        let det = model(&files.det)?;
        let rec = model(&files.rec)?;
        let dict = ocr_files::dict(&std::fs::read_to_string(&files.dict)?)?;
        Ok(Engine { api, env, det, rec, dict })
    }

    /// The text in the w×h BGRX image (rows `stride` bytes apart), in reading order.
    pub fn recognize(&self, bgrx: &[u8], w: usize, h: usize, stride: usize) -> Res<Vec<Line>> {
        if w == 0 || h == 0 {
            return Ok(vec![]);
        }
        let img = Img::from_bgrx(bgrx, w, h, stride);
        let boxes = self.detect(&img)?;
        let mut lines = Vec::with_capacity(boxes.len());
        for b in boxes {
            let (text, conf) = self.read(&img.crop(b))?;
            if conf >= TEXT_THRESH && !text.is_empty() {
                lines.push(Line { x: b.x0, y: b.y0, w: b.x1 - b.x0, h: b.y1 - b.y0, text, conf });
            }
        }
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        unsafe {
            libc::malloc_trim(0); // the tensors are gone: give the pages back (idle RSS 190 -> 90 MB)
        }
        Ok(lines)
    }

    /// Text boxes, top to bottom then left to right.
    fn detect(&self, img: &Img) -> Res<Vec<Box_>> {
        let (w, h) = (img.w as f32, img.h as f32);
        let ratio = (DET_MIN_SIDE / w.min(h)).min(DET_MAX_SIDE / w.max(h)).max(1.0);
        let side = |v: f32| ((v * ratio / 32.0).round() as usize).max(1) * 32;
        let (dw, dh) = (side(w), side(h));
        let scaled;
        let dimg = if (dw, dh) == (img.w, img.h) {
            img
        } else {
            scaled = img.resize(dw, dh);
            &scaled
        };
        let tensor = dimg.tensor([0.485, 0.456, 0.406], [0.229, 0.224, 0.225], dw);
        let (dims, prob) = self.run(&self.det, tensor, &[1, 3, dh as i64, dw as i64])?;
        let (pw, ph) = (dims[3] as usize, dims[2] as usize);
        Ok(db_boxes(&prob, pw, ph, (pw as f32 / w, ph as f32 / h), img.w, img.h))
    }

    /// One line's text and confidence.
    fn read(&self, crop: &Img) -> Res<(String, f32)> {
        let rw = ((REC_H as f32 * crop.w as f32 / crop.h as f32).ceil() as usize).max(1);
        let pad_w = rw.max(REC_MIN_W);
        let tensor = crop.resize(rw.min(pad_w), REC_H).tensor([0.5; 3], [0.5; 3], pad_w);
        let (dims, out) = self.run(&self.rec, tensor, &[1, 3, REC_H as i64, pad_w as i64])?;
        let (steps, classes) = (dims[1] as usize, dims[2] as usize);
        if classes != self.dict.len() + 2 {
            return Err(tr!("the recognizer and its dictionary don't match: {}", "el reconocedor y su diccionario no coinciden: {}", "認識器と辞書が一致しません: {}", reinstall()).into());
        }
        // CTC: the most likely class per step; repeats collapse; 0 is "nothing here".
        let (mut text, mut last, mut conf, mut n) = (String::new(), 0usize, 0f32, 0usize);
        for row in out.chunks_exact(classes).take(steps) {
            let (bi, bp) = row.iter().enumerate().fold((0, f32::MIN), |m, (i, &p)| if p > m.1 { (i, p) } else { m });
            if bi != 0 && bi != last {
                (conf, n) = (conf + bp, n + 1);
                match self.dict.get(bi - 1) {
                    Some(c) => text.push_str(c),
                    None => text.push(' '),
                }
            }
            last = bi;
        }
        Ok((text.trim().to_owned(), if n > 0 { conf / n as f32 } else { 0.0 }))
    }

    /// Run `s` on an f32 tensor of `shape`; its first output as (dims, data).
    fn run(&self, s: &Session, mut data: Vec<f32>, shape: &[i64]) -> Res<(Vec<i64>, Vec<f32>)> {
        let api = &self.api;
        let mut mem: *mut OrtMemoryInfo = null_mut();
        ort!(api, CreateCpuMemoryInfo(OrtDeviceAllocator, OrtMemTypeDefault, &mut mem))?;
        let mut input: *mut OrtValue = null_mut();
        let made = ort!(api, CreateTensorWithDataAsOrtValue(mem, data.as_mut_ptr().cast(), data.len() * 4, shape.as_ptr(), shape.len(), ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT, &mut input));
        unsafe { api.0.ReleaseMemoryInfo.unwrap()(mem) };
        made?;
        let (ins, outs, inputs) = ([s.input.as_ptr()], [s.output.as_ptr()], [input as *const OrtValue]);
        let mut output: *mut OrtValue = null_mut();
        let ran = ort!(api, Run(s.sess, std::ptr::null(), ins.as_ptr(), inputs.as_ptr(), 1, outs.as_ptr(), 1, &mut output));
        unsafe { api.0.ReleaseValue.unwrap()(input) };
        ran?;
        let res = (|| -> Res<(Vec<i64>, Vec<f32>)> {
            let mut info: *mut OrtTensorTypeAndShapeInfo = null_mut();
            ort!(api, GetTensorTypeAndShape(output, &mut info))?;
            let mut nd = 0usize;
            let dims = ort!(api, GetDimensionsCount(info, &mut nd)).and_then(|()| {
                let mut dims = vec![0i64; nd];
                ort!(api, GetDimensions(info, dims.as_mut_ptr(), nd))?;
                Ok(dims)
            });
            unsafe { api.0.ReleaseTensorTypeAndShapeInfo.unwrap()(info) };
            let dims = dims?;
            let n: usize = dims.iter().map(|&d| d.max(0) as usize).product();
            let mut p: *mut c_void = null_mut();
            ort!(api, GetTensorMutableData(output, &mut p))?;
            Ok((dims, unsafe { std::slice::from_raw_parts(p as *const f32, n) }.to_vec()))
        })();
        unsafe { api.0.ReleaseValue.unwrap()(output) };
        res
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        unsafe {
            self.api.0.ReleaseSession.unwrap()(self.det.sess);
            self.api.0.ReleaseSession.unwrap()(self.rec.sess);
            self.api.0.ReleaseEnv.unwrap()(self.env);
        }
    }
}

/// A session over the model `bytes`, set up to be a good neighbour: two threads that sleep
/// between jobs instead of spinning, no memory arena (it doubled the peak for nothing), CPU only.
fn session(api: &Api, env: *mut OrtEnv, bytes: &[u8]) -> Res<Session> {
    let mut so: *mut OrtSessionOptions = null_mut();
    ort!(api, CreateSessionOptions(&mut so))?;
    let res = (|| -> Res<*mut OrtSession> {
        ort!(api, SetIntraOpNumThreads(so, THREADS))?;
        ort!(api, SetInterOpNumThreads(so, 1))?;
        ort!(api, SetSessionGraphOptimizationLevel(so, ORT_ENABLE_ALL))?;
        ort!(api, AddSessionConfigEntry(so, c"session.intra_op.allow_spinning".as_ptr(), c"0".as_ptr()))?;
        ort!(api, AddSessionConfigEntry(so, c"session.inter_op.allow_spinning".as_ptr(), c"0".as_ptr()))?;
        ort!(api, DisableCpuMemArena(so))?;
        let mut sess: *mut OrtSession = null_mut();
        ort!(api, CreateSessionFromArray(env, bytes.as_ptr().cast(), bytes.len(), so, &mut sess))?;
        Ok(sess)
    })();
    unsafe { api.0.ReleaseSessionOptions.unwrap()(so) };
    let sess = res?;
    let mut alloc: *mut OrtAllocator = null_mut();
    ort!(api, GetAllocatorWithDefaultOptions(&mut alloc))?;
    let name = |f: unsafe extern "C" fn(*const OrtSession, usize, *mut OrtAllocator, *mut *mut c_char) -> *mut OrtStatus| -> Res<CString> {
        let mut p: *mut c_char = null_mut();
        api.ok(unsafe { f(sess, 0, alloc, &mut p) }, "SessionGetInputName")?;
        let s = unsafe { CStr::from_ptr(p) }.to_owned();
        unsafe { api.0.AllocatorFree.unwrap()(alloc, p.cast()) };
        Ok(s)
    };
    Ok(Session { sess, input: name(api.0.SessionGetInputName.unwrap())?, output: name(api.0.SessionGetOutputName.unwrap())? })
}

/// A BGR image, 3 bytes per pixel.
struct Img {
    w: usize,
    h: usize,
    px: Vec<u8>,
}

impl Img {
    fn from_bgrx(bgrx: &[u8], w: usize, h: usize, stride: usize) -> Img {
        let mut px = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for p in bgrx[y * stride..][..w * 4].as_chunks::<4>().0 {
                px.extend_from_slice(&p[..3]);
            }
        }
        Img { w, h, px }
    }

    /// Bilinear, pixel centres aligned (what OpenCV's INTER_LINEAR does).
    fn resize(&self, w: usize, h: usize) -> Img {
        let mut px = vec![0u8; w * h * 3];
        let (sx, sy) = (self.w as f32 / w as f32, self.h as f32 / h as f32);
        // Horizontal taps once per output column.
        let xs: Vec<(usize, usize, f32)> = (0..w)
            .map(|x| {
                let fx = ((x as f32 + 0.5) * sx - 0.5).max(0.0);
                let x0 = (fx as usize).min(self.w - 1);
                (x0, (x0 + 1).min(self.w - 1), fx - x0 as f32)
            })
            .collect();
        for y in 0..h {
            let fy = ((y as f32 + 0.5) * sy - 0.5).max(0.0);
            let y0 = (fy as usize).min(self.h - 1);
            let (y1, ty) = ((y0 + 1).min(self.h - 1), fy - y0 as f32);
            let (r0, r1) = (&self.px[y0 * self.w * 3..][..self.w * 3], &self.px[y1 * self.w * 3..][..self.w * 3]);
            let out = &mut px[y * w * 3..][..w * 3];
            for (x, &(x0, x1, tx)) in xs.iter().enumerate() {
                for c in 0..3 {
                    let top = r0[x0 * 3 + c] as f32 * (1.0 - tx) + r0[x1 * 3 + c] as f32 * tx;
                    let bot = r1[x0 * 3 + c] as f32 * (1.0 - tx) + r1[x1 * 3 + c] as f32 * tx;
                    out[x * 3 + c] = (top * (1.0 - ty) + bot * ty + 0.5) as u8;
                }
            }
        }
        Img { w, h, px }
    }

    /// NCHW f32 planes `pad_w` wide (zeros past the image), (x/255 - mean) / std per channel.
    fn tensor(&self, mean: [f32; 3], std: [f32; 3], pad_w: usize) -> Vec<f32> {
        let plane = pad_w * self.h;
        let mut out = vec![0f32; 3 * plane];
        for c in 0..3 {
            let (m, s) = (mean[c], 1.0 / (255.0 * std[c]));
            for y in 0..self.h {
                let row = &self.px[y * self.w * 3..][..self.w * 3];
                let dst = &mut out[c * plane + y * pad_w..][..self.w];
                for (d, p) in dst.iter_mut().zip(row.iter().skip(c).step_by(3)) {
                    *d = (*p as f32 - 255.0 * m) * s;
                }
            }
        }
        out
    }

    fn crop(&self, b: Box_) -> Img {
        let (w, h) = (b.x1 - b.x0, b.y1 - b.y0);
        let mut px = Vec::with_capacity(w * h * 3);
        for y in b.y0..b.y1 {
            px.extend_from_slice(&self.px[(y * self.w + b.x0) * 3..(y * self.w + b.x1) * 3]);
        }
        Img { w, h, px }
    }
}

/// A text box in image pixels, x1/y1 exclusive.
#[derive(Clone, Copy, Debug)]
struct Box_ {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
}

/// DB post-processing: threshold the probability map, take each connected blob's bounding
/// box, keep the confident ones, grow them back to the text's full size (the map marks a
/// shrunken core) and map them to image pixels; reading order.
fn db_boxes(prob: &[f32], w: usize, h: usize, scale: (f32, f32), ow: usize, oh: usize) -> Vec<Box_> {
    let bin: Vec<bool> = prob.iter().map(|&p| p > DET_THRESH).collect();
    let mut seen = vec![false; w * h];
    let (mut boxes, mut stack) = (vec![], vec![]);
    for start in 0..w * h {
        if !bin[start] || seen[start] {
            continue;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0usize, 0usize);
        stack.push(start);
        seen[start] = true;
        while let Some(i) = stack.pop() {
            let (x, y) = (i % w, i / w);
            (x0, x1, y0, y1) = (x0.min(x), x1.max(x), y0.min(y), y1.max(y));
            for (dx, dy) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1), (1, 1), (1, -1), (-1, 1), (-1, -1)] {
                let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                    continue;
                }
                let j = ny as usize * w + nx as usize;
                if bin[j] && !seen[j] {
                    seen[j] = true;
                    stack.push(j);
                }
            }
        }
        let (bw, bh) = (x1 - x0 + 1, y1 - y0 + 1);
        if bw < 3 || bh < 3 {
            continue;
        }
        let sum: f32 = (y0..=y1).map(|y| prob[y * w + x0..=y * w + x1].iter().sum::<f32>()).sum();
        if (sum / (bw * bh) as f32) < BOX_THRESH {
            continue;
        }
        let d = (bw * bh) as f32 * UNCLIP / (2.0 * (bw + bh) as f32);
        let fx = |v: f32| (v / scale.0).round().clamp(0.0, ow as f32) as usize;
        let fy = |v: f32| (v / scale.1).round().clamp(0.0, oh as f32) as usize;
        let b = Box_ { x0: fx(x0 as f32 - d), y0: fy(y0 as f32 - d), x1: fx(x1 as f32 + 1.0 + d), y1: fy(y1 as f32 + 1.0 + d) };
        if b.x1 > b.x0 + 2 && b.y1 > b.y0 + 2 {
            boxes.push(b);
        }
    }
    boxes.sort_by_key(|b| (b.y0 + b.y1, b.x0));
    boxes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boxes_come_out_of_the_map_in_reading_order() {
        // Two blobs on a 64x32 map: one at the bottom left, one at the top right.
        let (w, h) = (64, 32);
        let mut prob = vec![0f32; w * h];
        for y in 20..26 {
            for x in 4..30 {
                prob[y * w + x] = 0.9;
            }
        }
        for y in 4..10 {
            for x in 40..60 {
                prob[y * w + x] = 0.8;
            }
        }
        let b = db_boxes(&prob, w, h, (1.0, 1.0), w, h);
        assert_eq!(b.len(), 2);
        assert!(b[0].y0 < b[1].y0 && b[0].x0 > b[1].x0, "{b:?}");
        // Grown past the blob, clamped to the image.
        assert!(b[1].x0 < 4 && b[1].x1 > 30 && b[1].y0 < 20 && b[1].y1 > 26 && b[1].y1 <= h, "{b:?}");
        // A faint blob is dropped.
        prob.iter_mut().for_each(|p| *p *= 0.5);
        assert!(db_boxes(&prob, w, h, (1.0, 1.0), w, h).is_empty());
    }

    #[test]
    fn resize_keeps_flat_colours_and_tensor_normalizes() {
        let img = Img { w: 4, h: 2, px: [10u8, 20, 30].repeat(8) };
        let r = img.resize(9, 5);
        assert!(r.px.chunks(3).all(|p| p == [10, 20, 30]));
        let t = img.tensor([0.5; 3], [0.5; 3], 6);
        assert_eq!(t.len(), 3 * 6 * 2);
        assert!((t[0] - (10.0 / 255.0 - 0.5) / 0.5).abs() < 1e-6);
        assert_eq!(t[4], 0.0, "padding past the image is zero");
        assert!((t[6 * 2] - (20.0 / 255.0 - 0.5) / 0.5).abs() < 1e-6, "second plane is the G channel");
    }

    /// This process's CPU time so far (all threads), in ms; 0 where there's no getrusage (Windows).
    fn cpu_ms() -> u64 {
        #[cfg(unix)]
        {
            let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
            unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
            (ru.ru_utime.tv_sec + ru.ru_stime.tv_sec) as u64 * 1000 + (ru.ru_utime.tv_usec + ru.ru_stime.tv_usec) as u64 / 1000
        }
        #[cfg(not(unix))]
        0
    }

    /// A PNG as BGRX.
    fn load_png(path: &std::path::Path) -> (Vec<u8>, usize, usize) {
        let dec = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()));
        let mut r = dec.read_info().unwrap();
        let mut buf = vec![0; r.output_buffer_size().unwrap()];
        let info = r.next_frame(&mut buf).unwrap();
        let (w, h, n) = (info.width as usize, info.height as usize, info.color_type.samples());
        (buf.chunks(n).flat_map(|p| [p[2], p[1], p[0], 0]).collect(), w, h)
    }

    /// Needs the installed files (see ocr_files): reads the sample next to the sources, with
    /// Spanish accents and Japanese, and checks the text. CI runs it after the install test.
    #[test]
    #[ignore]
    fn ocr_reads_spanish_and_japanese_from_the_sample() {
        let engine = Engine::load(&ocr_files::files().unwrap()).unwrap();
        let (bgrx, w, h) = load_png(std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ocr-sample.png")));
        let lines = engine.recognize(&bgrx, w, h, w * 4).unwrap();
        let text = text_of(&lines);
        assert!(text.contains("Hola señor, ¿qué tal?"), "{text}");
        assert!(text.contains("日本語のテキスト"), "{text}");
    }

    /// Scores the shipped pipeline on a folder of screenshots: SCREENREC_OCR_TESTSET=<dir>
    /// reads its *.png and writes <dir>/rust-results.tsv (image, x, y, w, h, confidence, text;
    /// a `#` line per image with its milliseconds) for a scorer with the ground truth.
    #[test]
    #[ignore]
    fn ocr_dumps_a_testset() {
        let Some(dir) = std::env::var_os("SCREENREC_OCR_TESTSET").map(std::path::PathBuf::from) else { return };
        let engine = Engine::load(&ocr_files::files().unwrap()).unwrap();
        let mut out = String::new();
        let mut names: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "png")).collect();
        names.sort();
        for p in names {
            let (bgrx, w, h) = load_png(&p);
            let (t, c0) = (std::time::Instant::now(), cpu_ms());
            let lines = engine.recognize(&bgrx, w, h, w * 4).unwrap();
            let name = p.file_name().unwrap().to_string_lossy();
            out.push_str(&format!("# {name} {} ms {} lines {} cpu_ms {}x{}\n", t.elapsed().as_millis(), lines.len(), cpu_ms() - c0, w, h));
            for l in lines {
                out.push_str(&format!("{name}\t{}\t{}\t{}\t{}\t{:.3}\t{}\n", l.x, l.y, l.w, l.h, l.conf, l.text));
            }
        }
        std::fs::write(dir.join("rust-results.tsv"), out).unwrap();
    }
}
