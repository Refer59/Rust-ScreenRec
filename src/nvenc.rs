//! H.264 on NVENC, fed from CUDA memory. The X capture buffer is pinned, so
//! uploading the changed rows is a DMA the CPU never touches, and NVENC eats
//! BGRX as-is (the RGB->YUV conversion runs on the GPU). The driver is loaded
//! at runtime: without one the program still runs, and records on the CPU.

use crate::Res;
use crate::nvenc_sys::*;
use std::ffi::{CStr, c_char, c_void};
use std::mem::{transmute_copy, zeroed};
use std::ptr::null_mut;
use std::sync::OnceLock;

#[repr(C)]
struct CudaMemcpy2D {
    src_x_in_bytes: usize,
    src_y: usize,
    src_memory_type: u32,
    src_host: *const c_void,
    src_device: u64,
    src_array: *mut c_void,
    src_pitch: usize,
    dst_x_in_bytes: usize,
    dst_y: usize,
    dst_memory_type: u32,
    dst_host: *mut c_void,
    dst_device: u64,
    dst_array: *mut c_void,
    dst_pitch: usize,
    width_in_bytes: usize,
    height: usize,
}
const CU_MEMORYTYPE_HOST: u32 = 1;
const CU_MEMORYTYPE_DEVICE: u32 = 2;
const CU_CTX_SCHED_BLOCKING_SYNC: u32 = 4;

/// The NVIDIA driver's entry points we use.
struct Driver {
    init: unsafe extern "C" fn(u32) -> i32,
    device_get: unsafe extern "C" fn(*mut i32, i32) -> i32,
    ctx_create: unsafe extern "C" fn(*mut *mut c_void, u32, i32) -> i32,
    ctx_destroy: unsafe extern "C" fn(*mut c_void) -> i32,
    alloc_pitch: unsafe extern "C" fn(*mut u64, *mut usize, usize, usize, u32) -> i32,
    free: unsafe extern "C" fn(u64) -> i32,
    host_register: unsafe extern "C" fn(*mut c_void, usize, u32) -> i32,
    host_unregister: unsafe extern "C" fn(*mut c_void) -> i32,
    memcpy_2d: unsafe extern "C" fn(*const CudaMemcpy2D) -> i32,
    error_string: unsafe extern "C" fn(i32, *mut *const c_char) -> i32,
    max_version: unsafe extern "C" fn(*mut u32) -> NVENCSTATUS,
    create_instance: unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS,
}

/// `name` from `lib` as the function pointer type `F`.
unsafe fn sym<F>(lib: *mut c_void, name: &CStr) -> Option<F> {
    let p = unsafe { libc::dlsym(lib, name.as_ptr()) };
    (!p.is_null()).then(|| unsafe { transmute_copy::<*mut c_void, F>(&p) })
}

fn driver() -> Option<&'static Driver> {
    static D: OnceLock<Option<Driver>> = OnceLock::new();
    let load = || unsafe {
        let open = |lib: &CStr| Some(libc::dlopen(lib.as_ptr(), libc::RTLD_NOW)).filter(|h| !h.is_null());
        let (cuda, nvenc) = (open(c"libcuda.so.1")?, open(c"libnvidia-encode.so.1")?);
        Some(Driver {
            init: sym(cuda, c"cuInit")?,
            device_get: sym(cuda, c"cuDeviceGet")?,
            ctx_create: sym(cuda, c"cuCtxCreate_v2")?,
            ctx_destroy: sym(cuda, c"cuCtxDestroy_v2")?,
            alloc_pitch: sym(cuda, c"cuMemAllocPitch_v2")?,
            free: sym(cuda, c"cuMemFree_v2")?,
            host_register: sym(cuda, c"cuMemHostRegister_v2")?,
            host_unregister: sym(cuda, c"cuMemHostUnregister")?,
            memcpy_2d: sym(cuda, c"cuMemcpy2D_v2")?,
            error_string: sym(cuda, c"cuGetErrorString")?,
            max_version: sym(nvenc, c"NvEncodeAPIGetMaxSupportedVersion")?,
            create_instance: sym(nvenc, c"NvEncodeAPICreateInstance")?,
        })
    };
    D.get_or_init(load).as_ref()
}

/// Whether an NVIDIA driver with NVENC is installed (cheap: doesn't wake the GPU).
pub fn available() -> bool {
    driver().is_some()
}

fn cu(d: &Driver, r: i32, what: &str) -> Res<()> {
    if r == 0 {
        return Ok(());
    }
    let mut s = std::ptr::null();
    unsafe { (d.error_string)(r, &mut s) };
    let msg = if s.is_null() { "?".into() } else { unsafe { CStr::from_ptr(s) }.to_string_lossy() };
    Err(format!("CUDA {what}: {msg}").into())
}

const API: u32 = NVENCAPI_MAJOR_VERSION | NVENCAPI_MINOR_VERSION << 24;
const fn ver(v: u32) -> u32 {
    API | v << 16 | 7 << 28
}
const CODEC_H264: GUID =
    GUID { Data1: 0x6bc82762, Data2: 0x4e63, Data3: 0x4ca4, Data4: [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf] };
const PRESET_P4: GUID =
    GUID { Data1: 0x90a7b826, Data2: 0xdf06, Data3: 0x4862, Data4: [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81] };

/// Constant QP keeps text sharp, and frames where little changed cost almost nothing.
const QP: u32 = 22;

macro_rules! nv {
    ($s:expr, $f:ident($($a:expr),*)) => {
        $s.check(#[allow(unused_unsafe)] unsafe { $s.api.$f.unwrap()($s.enc, $($a),*) }, stringify!($f))
    };
}

pub struct Encoder {
    api: NV_ENCODE_API_FUNCTION_LIST,
    enc: *mut c_void,
    ctx: *mut c_void,
    pinned: *mut c_void,
    dev: u64,
    pitch: usize,
    w: u32,
    h: u32,
    reg: NV_ENC_REGISTERED_PTR,
    out: NV_ENC_OUTPUT_PTR,
}

impl Encoder {
    /// Encodes the top-left `w`×`h` of `host`, the BGRX capture buffer.
    pub fn new(host: &[u8], w: usize, h: usize, fps: u32) -> Res<Self> {
        let (w, h) = (w as u32, h as u32);
        let mut e = Encoder {
            api: unsafe { zeroed() },
            enc: null_mut(),
            ctx: null_mut(),
            pinned: null_mut(),
            dev: 0,
            pitch: 0,
            w,
            h,
            reg: null_mut(),
            out: null_mut(),
        };
        let d = driver().ok_or("no hay driver NVIDIA (NVENC)")?;
        unsafe {
            let mut dev = 0;
            cu(d, (d.init)(0), "cuInit")?;
            cu(d, (d.device_get)(&mut dev, 0), "cuDeviceGet")?;
            // Block (don't spin) while waiting for DMA: the wait is free, a spin burns a core.
            cu(d, (d.ctx_create)(&mut e.ctx, CU_CTX_SCHED_BLOCKING_SYNC, dev), "cuCtxCreate")?;
            cu(d, (d.alloc_pitch)(&mut e.dev, &mut e.pitch, w as usize * 4, h as usize, 16), "cuMemAllocPitch")?;
            // Pinned memory is copied by DMA; unpinned still works, through an extra CPU copy.
            if (d.host_register)(host.as_ptr() as *mut c_void, host.len(), 0) == 0 {
                e.pinned = host.as_ptr() as *mut c_void;
            } else {
                eprintln!("aviso: no se pudo fijar el buffer de captura; subir frames a la GPU costará más CPU");
            }

            let mut max = 0;
            (d.max_version)(&mut max);
            if max < (NVENCAPI_MAJOR_VERSION << 4 | NVENCAPI_MINOR_VERSION) {
                return Err("driver NVIDIA demasiado viejo: hace falta NVENC 12.1 (driver 530+)".into());
            }
            e.api.version = ver(2);
            if (d.create_instance)(&mut e.api) != NV_ENC_SUCCESS {
                return Err("NvEncodeAPICreateInstance falló".into());
            }
            let mut p: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = zeroed();
            p.version = ver(1);
            p.deviceType = NV_ENC_DEVICE_TYPE_CUDA;
            p.device = e.ctx;
            p.apiVersion = API;
            let st = e.api.nvEncOpenEncodeSessionEx.unwrap()(&mut p, &mut e.enc);
            e.check(st, "nvEncOpenEncodeSessionEx")?;

            let mut preset: NV_ENC_PRESET_CONFIG = zeroed();
            preset.version = ver(4) | 1 << 31;
            preset.presetCfg.version = ver(8) | 1 << 31;
            nv!(e, nvEncGetEncodePresetConfigEx(CODEC_H264, PRESET_P4, NV_ENC_TUNING_INFO_HIGH_QUALITY, &mut preset))?;
            let mut cfg = preset.presetCfg;
            cfg.gopLength = NVENC_INFINITE_GOPLENGTH; // keyframes are forced by wall time instead
            cfg.frameIntervalP = 1; // no B-frames: output order == capture order, zero delay
            cfg.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CONSTQP;
            cfg.rcParams.constQP = NV_ENC_QP { qpInterP: QP, qpInterB: QP, qpIntra: QP - 2 };
            let h264 = &mut cfg.encodeCodecConfig.h264Config;
            h264.idrPeriod = NVENC_INFINITE_GOPLENGTH;
            let vui = &mut h264.h264VUIParameters;
            vui.videoSignalTypePresentFlag = 1;
            vui.videoFormat = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
            vui.colourDescriptionPresentFlag = 1;
            vui.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES_BT709;
            vui.transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
            vui.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS_BT709;

            let mut init: NV_ENC_INITIALIZE_PARAMS = zeroed();
            init.version = ver(6) | 1 << 31;
            init.encodeGUID = CODEC_H264;
            init.presetGUID = PRESET_P4;
            init.tuningInfo = NV_ENC_TUNING_INFO_HIGH_QUALITY;
            (init.encodeWidth, init.encodeHeight, init.darWidth, init.darHeight) = (w, h, w, h);
            (init.maxEncodeWidth, init.maxEncodeHeight) = (w, h);
            (init.frameRateNum, init.frameRateDen) = (fps, 1);
            init.enablePTD = 1;
            init.encodeConfig = &mut cfg;
            nv!(e, nvEncInitializeEncoder(&mut init))?;

            let mut reg: NV_ENC_REGISTER_RESOURCE = zeroed();
            reg.version = ver(4);
            reg.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR;
            (reg.width, reg.height, reg.pitch) = (w, h, e.pitch as u32);
            reg.resourceToRegister = e.dev as *mut c_void;
            reg.bufferFormat = NV_ENC_BUFFER_FORMAT_ARGB; // bytes B,G,R,X: X11's own layout
            reg.bufferUsage = NV_ENC_INPUT_IMAGE;
            nv!(e, nvEncRegisterResource(&mut reg))?;
            e.reg = reg.registeredResource;

            let mut bs: NV_ENC_CREATE_BITSTREAM_BUFFER = zeroed();
            bs.version = ver(1);
            nv!(e, nvEncCreateBitstreamBuffer(&mut bs))?;
            e.out = bs.bitstreamBuffer;
        }
        Ok(e)
    }

    fn check(&self, st: NVENCSTATUS, what: &str) -> Res<()> {
        if st == NV_ENC_SUCCESS {
            return Ok(());
        }
        let mut detail = String::new();
        if let (Some(f), false) = (self.api.nvEncGetLastErrorString, self.enc.is_null()) {
            let s = unsafe { f(self.enc) };
            if !s.is_null() {
                detail = unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned();
            }
        }
        Err(format!("NVENC {what}: error {st} {detail}").into())
    }

    /// DMA rows [y0, y1) of `host` (`stride` bytes per row) to the GPU frame.
    pub fn upload(&mut self, host: &[u8], stride: usize, (y0, y1): (i32, i32)) -> Res<()> {
        let y1 = y1.min(self.h as i32);
        if y0 >= y1 {
            return Ok(());
        }
        let c = CudaMemcpy2D {
            src_y: y0 as usize,
            src_memory_type: CU_MEMORYTYPE_HOST,
            src_host: host.as_ptr().cast(),
            src_pitch: stride,
            dst_y: y0 as usize,
            dst_memory_type: CU_MEMORYTYPE_DEVICE,
            dst_device: self.dev,
            dst_pitch: self.pitch,
            width_in_bytes: self.w as usize * 4,
            height: (y1 - y0) as usize,
            ..unsafe { zeroed() }
        };
        let d = driver().ok_or("no hay driver NVIDIA")?;
        cu(d, unsafe { (d.memcpy_2d)(&c) }, "cuMemcpy2D")
    }

    /// Encode the GPU frame into `out` (Annex B). Returns whether it is a keyframe.
    pub fn encode(&mut self, idr: bool, out: &mut Vec<u8>) -> Res<bool> {
        let mut map: NV_ENC_MAP_INPUT_RESOURCE = unsafe { zeroed() };
        map.version = ver(4);
        map.registeredResource = self.reg;
        nv!(self, nvEncMapInputResource(&mut map))?;

        let mut pic: NV_ENC_PIC_PARAMS = unsafe { zeroed() };
        pic.version = ver(6) | 1 << 31;
        (pic.inputWidth, pic.inputHeight, pic.inputPitch) = (self.w, self.h, self.pitch as u32);
        pic.inputBuffer = map.mappedResource;
        pic.bufferFmt = map.mappedBufferFmt;
        pic.outputBitstream = self.out;
        pic.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
        if idr {
            pic.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR;
        }
        let res = nv!(self, nvEncEncodePicture(&mut pic)).and_then(|()| {
            let mut lock: NV_ENC_LOCK_BITSTREAM = unsafe { zeroed() };
            lock.version = ver(1) | 1 << 31;
            lock.outputBitstream = self.out;
            nv!(self, nvEncLockBitstream(&mut lock))?;
            out.clear();
            out.extend_from_slice(unsafe {
                std::slice::from_raw_parts(lock.bitstreamBufferPtr.cast(), lock.bitstreamSizeInBytes as usize)
            });
            nv!(self, nvEncUnlockBitstream(self.out))?;
            Ok(lock.pictureType == NV_ENC_PIC_TYPE_IDR)
        });
        nv!(self, nvEncUnmapInputResource(map.mappedResource))?;
        res
    }
}

// Not optional: with the session still open, the NVIDIA driver deadlocks the
// process in its exit handlers.
impl Drop for Encoder {
    fn drop(&mut self) {
        let Some(d) = driver() else { return };
        unsafe {
            if !self.enc.is_null() {
                if !self.out.is_null() {
                    self.api.nvEncDestroyBitstreamBuffer.unwrap()(self.enc, self.out);
                }
                if !self.reg.is_null() {
                    self.api.nvEncUnregisterResource.unwrap()(self.enc, self.reg);
                }
                self.api.nvEncDestroyEncoder.unwrap()(self.enc);
            }
            if self.dev != 0 {
                (d.free)(self.dev);
            }
            if !self.pinned.is_null() {
                (d.host_unregister)(self.pinned);
            }
            if !self.ctx.is_null() {
                (d.ctx_destroy)(self.ctx);
            }
        }
    }
}
