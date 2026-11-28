//! Runtime-loaded (`dlopen`) FFmpeg (`libavcodec` 60/61/62 + its linked `libavutil` 58/59/60).
//!
//! All symbols are looked up through a single `libavcodec.so.N` handle so `avutil` always matches
//! the exact library `libavcodec` was linked against. Struct field access goes through `ffmpeg_abi.c`
//! (compiled by `cc` in `build.rs`) and runtime `av_opt_find` anchor checks.

use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::sync::OnceLock;

use anyhow::{Result, anyhow, bail};
use tracing::{debug, info, warn};

/// `AV_OPT_SEARCH_CHILDREN`
pub(super) const AV_OPT_SEARCH_CHILDREN: c_int = 1;
/// `AV_PICTURE_TYPE_NONE`
pub(super) const AV_PICTURE_TYPE_NONE: c_int = 0;
/// `AV_PICTURE_TYPE_I`
pub(super) const AV_PICTURE_TYPE_I: c_int = 1;
/// `AV_PKT_FLAG_KEY`
pub(super) const AV_PKT_FLAG_KEY: c_int = 1;
/// `AVERROR(EAGAIN)`
pub(super) const AVERROR_EAGAIN: c_int = -libc::EAGAIN;

/// Prefix of FFmpeg's `AVOption` struct (unchanged across FFmpeg 4–8).
#[repr(C)]
struct AvOption {
    name: *const c_char,
    help: *const c_char,
    offset: c_int,
    opt_type: c_int,
}

unsafe extern "C" {
    fn ah_av_hwframes_configure(
        frames_ref: *mut c_void,
        avutil_major: c_int,
        pool_size: c_int,
        hw_fmt: c_int,
        sw_fmt: c_int,
        w: c_int,
        h: c_int,
    ) -> c_int;
    fn ah_av_frame_set_params(
        frame: *mut c_void,
        avutil_major: c_int,
        w: c_int,
        h: c_int,
        format: c_int,
    ) -> c_int;
    fn ah_av_frame_plane(
        frame: *mut c_void,
        avutil_major: c_int,
        plane: c_int,
        linesize_out: *mut c_int,
    ) -> *mut u8;
    fn ah_av_frame_set_pict_type(
        frame: *mut c_void,
        avutil_major: c_int,
        pict_type: c_int,
    ) -> c_int;
    fn ah_av_frame_set_pts(frame: *mut c_void, avutil_major: c_int, pts: i64) -> c_int;
    fn ah_av_packet_data(pkt: *const c_void) -> *const u8;
    fn ah_av_packet_size(pkt: *const c_void) -> c_int;
    fn ah_av_packet_flags(pkt: *const c_void) -> c_int;
    fn ah_av_ctx_hw_frames_offset(
        avcodec_major: c_int,
        off_max_pixels: c_int,
        off_hwaccel_flags: c_int,
        off_extra_hw_frames: c_int,
    ) -> c_int;
    fn ah_av_ctx_framerate_offset(
        avcodec_major: c_int,
        off_time_base: c_int,
        off_pkt_timebase: c_int,
    ) -> c_int;
    fn ah_av_ctx_set_hw_frames_ctx(ctx: *mut c_void, offset: c_int, frames_ref: *mut c_void);
    fn ah_av_ctx_set_framerate(ctx: *mut c_void, offset: c_int, num: c_int, den: c_int);
}

pub(super) struct FfmpegApi {
    _handle: *mut c_void,
    pub(super) avutil_major: c_int,
    pub(super) pix_fmt_vaapi: c_int,
    pub(super) pix_fmt_nv12: c_int,
    pub(super) hwdevice_vaapi: c_int,
    hw_frames_ctx_offset: c_int,
    framerate_offset: c_int,

    avcodec_find_encoder_by_name: unsafe extern "C" fn(*const c_char) -> *const c_void,
    avcodec_alloc_context3: unsafe extern "C" fn(*const c_void) -> *mut c_void,
    avcodec_open2: unsafe extern "C" fn(*mut c_void, *const c_void, *mut *mut c_void) -> c_int,
    avcodec_send_frame: unsafe extern "C" fn(*mut c_void, *const c_void) -> c_int,
    avcodec_receive_packet: unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int,
    avcodec_free_context: unsafe extern "C" fn(*mut *mut c_void),
    av_packet_alloc: unsafe extern "C" fn() -> *mut c_void,
    av_packet_unref: unsafe extern "C" fn(*mut c_void),
    av_packet_free: unsafe extern "C" fn(*mut *mut c_void),

    av_frame_alloc: unsafe extern "C" fn() -> *mut c_void,
    av_frame_get_buffer: unsafe extern "C" fn(*mut c_void, c_int) -> c_int,
    av_frame_make_writable: unsafe extern "C" fn(*mut c_void) -> c_int,
    av_frame_unref: unsafe extern "C" fn(*mut c_void),
    av_frame_free: unsafe extern "C" fn(*mut *mut c_void),

    av_hwdevice_ctx_create: unsafe extern "C" fn(
        *mut *mut c_void,
        c_int,
        *const c_char,
        *mut c_void,
        c_int,
    ) -> c_int,
    av_hwframe_ctx_alloc: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    av_hwframe_ctx_init: unsafe extern "C" fn(*mut c_void) -> c_int,
    av_hwframe_get_buffer: unsafe extern "C" fn(*mut c_void, *mut c_void, c_int) -> c_int,
    av_hwframe_transfer_data: unsafe extern "C" fn(*mut c_void, *const c_void, c_int) -> c_int,
    av_buffer_ref: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    av_buffer_unref: unsafe extern "C" fn(*mut *mut c_void),

    av_opt_find: unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        *const c_char,
        c_int,
        c_int,
    ) -> *const AvOption,
    av_opt_set: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, c_int) -> c_int,
    av_opt_set_int: unsafe extern "C" fn(*mut c_void, *const c_char, i64, c_int) -> c_int,
    av_opt_set_image_size:
        unsafe extern "C" fn(*mut c_void, *const c_char, c_int, c_int, c_int) -> c_int,
    av_opt_set_pixel_fmt: unsafe extern "C" fn(*mut c_void, *const c_char, c_int, c_int) -> c_int,
    av_strerror: unsafe extern "C" fn(c_int, *mut c_char, usize) -> c_int,
}

// The loaded shared library handle and function pointers are immutable process-wide state.
unsafe impl Send for FfmpegApi {}
unsafe impl Sync for FfmpegApi {}

unsafe fn load_sym<T>(handle: *mut c_void, name: &CStr) -> Result<T> {
    let ptr = unsafe { libc::dlsym(handle, name.as_ptr()) };
    if ptr.is_null() {
        bail!("missing FFmpeg symbol {}", name.to_string_lossy());
    }
    Ok(unsafe { std::mem::transmute_copy(&ptr) })
}

fn try_load_soname(soname: &CStr) -> Result<FfmpegApi> {
    let handle = unsafe { libc::dlopen(soname.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
    if handle.is_null() {
        let err = unsafe {
            let p = libc::dlerror();
            if p.is_null() {
                "dlopen failed".to_string()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        bail!("{}: {err}", soname.to_string_lossy());
    }
    let result = unsafe { init_from_handle(handle) };
    if result.is_err() {
        unsafe {
            libc::dlclose(handle);
        }
    }
    result
}

unsafe fn init_from_handle(handle: *mut c_void) -> Result<FfmpegApi> {
    unsafe {
        let avcodec_version: unsafe extern "C" fn() -> c_uint =
            load_sym(handle, c"avcodec_version")?;
        let avutil_version: unsafe extern "C" fn() -> c_uint = load_sym(handle, c"avutil_version")?;
        let avcodec_major = (avcodec_version() >> 16) as c_int;
        let avutil_major = (avutil_version() >> 16) as c_int;

        if !matches!((avcodec_major, avutil_major), (60, 58) | (61, 59) | (62, 60)) {
            bail!(
                "unsupported FFmpeg ABI pair libavcodec.{avcodec_major} / libavutil.{avutil_major}"
            );
        }

        let av_get_pix_fmt: unsafe extern "C" fn(*const c_char) -> c_int =
            load_sym(handle, c"av_get_pix_fmt")?;
        let av_hwdevice_find_type_by_name: unsafe extern "C" fn(*const c_char) -> c_int =
            load_sym(handle, c"av_hwdevice_find_type_by_name")?;

        let pix_fmt_vaapi = av_get_pix_fmt(c"vaapi".as_ptr());
        let pix_fmt_nv12 = av_get_pix_fmt(c"nv12".as_ptr());
        let hwdevice_vaapi = av_hwdevice_find_type_by_name(c"vaapi".as_ptr());
        if pix_fmt_nv12 < 0 {
            bail!("libavutil does not recognize nv12 pixel format");
        }

        let avcodec_alloc_context3: unsafe extern "C" fn(*const c_void) -> *mut c_void =
            load_sym(handle, c"avcodec_alloc_context3")?;
        let avcodec_free_context: unsafe extern "C" fn(*mut *mut c_void) =
            load_sym(handle, c"avcodec_free_context")?;
        let av_opt_find: unsafe extern "C" fn(
            *mut c_void,
            *const c_char,
            *const c_char,
            c_int,
            c_int,
        ) -> *const AvOption = load_sym(handle, c"av_opt_find")?;

        // Probe and validate AVCodecContext anchor offsets once at library load time.
        let mut probe_ctx = avcodec_alloc_context3(std::ptr::null());
        if probe_ctx.is_null() {
            bail!("avcodec_alloc_context3(NULL) returned null");
        }
        let opt_off = |name: &CStr| -> c_int {
            let opt = av_opt_find(probe_ctx, name.as_ptr(), std::ptr::null(), 0, 0);
            if opt.is_null() {
                -1
            } else {
                (*opt).offset
            }
        };
        let off_max_pixels = opt_off(c"max_pixels");
        let off_hwaccel_flags = opt_off(c"hwaccel_flags");
        let off_extra_hw_frames = opt_off(c"extra_hw_frames");
        let off_time_base = opt_off(c"time_base");
        let off_pkt_timebase = opt_off(c"pkt_timebase");
        avcodec_free_context(&mut probe_ctx);

        let hw_frames_ctx_offset = ah_av_ctx_hw_frames_offset(
            avcodec_major,
            off_max_pixels,
            off_hwaccel_flags,
            off_extra_hw_frames,
        );
        if hw_frames_ctx_offset < 0 {
            bail!(
                "AVCodecContext hw_frames_ctx anchor mismatch for libavcodec.{avcodec_major} \
                 (max_pixels={off_max_pixels}, hwaccel_flags={off_hwaccel_flags}, \
                  extra_hw_frames={off_extra_hw_frames})"
            );
        }
        let framerate_offset =
            ah_av_ctx_framerate_offset(avcodec_major, off_time_base, off_pkt_timebase);
        if framerate_offset < 0 {
            bail!(
                "AVCodecContext framerate anchor mismatch for libavcodec.{avcodec_major} \
                 (time_base={off_time_base}, pkt_timebase={off_pkt_timebase})"
            );
        }

        let api = FfmpegApi {
            _handle: handle,
            avutil_major,
            pix_fmt_vaapi,
            pix_fmt_nv12,
            hwdevice_vaapi,
            hw_frames_ctx_offset,
            framerate_offset,

            avcodec_find_encoder_by_name: load_sym(handle, c"avcodec_find_encoder_by_name")?,
            avcodec_alloc_context3,
            avcodec_open2: load_sym(handle, c"avcodec_open2")?,
            avcodec_send_frame: load_sym(handle, c"avcodec_send_frame")?,
            avcodec_receive_packet: load_sym(handle, c"avcodec_receive_packet")?,
            avcodec_free_context,
            av_packet_alloc: load_sym(handle, c"av_packet_alloc")?,
            av_packet_unref: load_sym(handle, c"av_packet_unref")?,
            av_packet_free: load_sym(handle, c"av_packet_free")?,

            av_frame_alloc: load_sym(handle, c"av_frame_alloc")?,
            av_frame_get_buffer: load_sym(handle, c"av_frame_get_buffer")?,
            av_frame_make_writable: load_sym(handle, c"av_frame_make_writable")?,
            av_frame_unref: load_sym(handle, c"av_frame_unref")?,
            av_frame_free: load_sym(handle, c"av_frame_free")?,

            av_hwdevice_ctx_create: load_sym(handle, c"av_hwdevice_ctx_create")?,
            av_hwframe_ctx_alloc: load_sym(handle, c"av_hwframe_ctx_alloc")?,
            av_hwframe_ctx_init: load_sym(handle, c"av_hwframe_ctx_init")?,
            av_hwframe_get_buffer: load_sym(handle, c"av_hwframe_get_buffer")?,
            av_hwframe_transfer_data: load_sym(handle, c"av_hwframe_transfer_data")?,
            av_buffer_ref: load_sym(handle, c"av_buffer_ref")?,
            av_buffer_unref: load_sym(handle, c"av_buffer_unref")?,

            av_opt_find,
            av_opt_set: load_sym(handle, c"av_opt_set")?,
            av_opt_set_int: load_sym(handle, c"av_opt_set_int")?,
            av_opt_set_image_size: load_sym(handle, c"av_opt_set_image_size")?,
            av_opt_set_pixel_fmt: load_sym(handle, c"av_opt_set_pixel_fmt")?,
            av_strerror: load_sym(handle, c"av_strerror")?,
        };
        info!(
            avcodec = avcodec_major,
            avutil = avutil_major,
            hw_frames_ctx_offset,
            framerate_offset,
            "loaded FFmpeg for Linux hardware encoding"
        );
        Ok(api)
    }
}

/// Process-wide FFmpeg handle (FFmpeg 8 -> 7 -> 6). Returns `None` if unavailable.
pub(super) fn api() -> Option<&'static FfmpegApi> {
    static API: OnceLock<Option<FfmpegApi>> = OnceLock::new();
    API.get_or_init(|| {
        for soname in [c"libavcodec.so.62", c"libavcodec.so.61", c"libavcodec.so.60"] {
            match try_load_soname(soname) {
                Ok(api) => return Some(api),
                Err(e) => debug!(soname = %soname.to_string_lossy(), "FFmpeg load skipped: {e:#}"),
            }
        }
        warn!(
            "no supported libavcodec (60/61/62) found; Linux hardware encoding unavailable, \
             using software OpenH264"
        );
        None
    })
    .as_ref()
}

impl FfmpegApi {
    pub(super) fn err_string(&self, errnum: c_int) -> String {
        let mut buf = [0u8; 128];
        let rc = unsafe { (self.av_strerror)(errnum, buf.as_mut_ptr().cast(), buf.len()) };
        if rc == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            String::from_utf8_lossy(&buf[..end]).into_owned()
        } else {
            format!("FFmpeg error {errnum}")
        }
    }

    pub(super) fn check(&self, what: &str, rc: c_int) -> Result<()> {
        if rc >= 0 {
            Ok(())
        } else {
            Err(anyhow!("{what}: {}", self.err_string(rc)))
        }
    }

    pub(super) fn find_encoder(&self, name: &CStr) -> Option<*const c_void> {
        let p = unsafe { (self.avcodec_find_encoder_by_name)(name.as_ptr()) };
        (!p.is_null()).then_some(p)
    }

    pub(super) fn alloc_context(&'static self, codec: *const c_void) -> Result<AvCodecContext> {
        let ptr = unsafe { (self.avcodec_alloc_context3)(codec) };
        if ptr.is_null() {
            bail!("avcodec_alloc_context3 returned null");
        }
        Ok(AvCodecContext { api: self, ptr })
    }

    pub(super) fn alloc_frame(&'static self) -> Result<AvFrame> {
        let ptr = unsafe { (self.av_frame_alloc)() };
        if ptr.is_null() {
            bail!("av_frame_alloc returned null");
        }
        Ok(AvFrame { api: self, ptr })
    }

    pub(super) fn alloc_packet(&'static self) -> Result<AvPacket> {
        let ptr = unsafe { (self.av_packet_alloc)() };
        if ptr.is_null() {
            bail!("av_packet_alloc returned null");
        }
        Ok(AvPacket { api: self, ptr })
    }

    pub(super) fn create_vaapi_device(&'static self, device_path: &str) -> Result<AvBufferRef> {
        if self.hwdevice_vaapi <= 0 || self.pix_fmt_vaapi < 0 {
            bail!("FFmpeg was built without VA-API support");
        }
        let path = CString::new(device_path)?;
        let mut ptr: *mut c_void = std::ptr::null_mut();
        let rc = unsafe {
            (self.av_hwdevice_ctx_create)(
                &mut ptr,
                self.hwdevice_vaapi,
                path.as_ptr(),
                std::ptr::null_mut(),
                0,
            )
        };
        self.check("av_hwdevice_ctx_create(vaapi)", rc)?;
        if ptr.is_null() {
            bail!("av_hwdevice_ctx_create returned null");
        }
        Ok(AvBufferRef { api: self, ptr })
    }

    pub(super) fn create_vaapi_frames(
        &'static self,
        device: &AvBufferRef,
        w: u32,
        h: u32,
        pool_size: i32,
    ) -> Result<AvBufferRef> {
        let ptr = unsafe { (self.av_hwframe_ctx_alloc)(device.ptr) };
        if ptr.is_null() {
            bail!("av_hwframe_ctx_alloc returned null");
        }
        let buf = AvBufferRef { api: self, ptr };
        let rc = unsafe {
            ah_av_hwframes_configure(
                buf.ptr,
                self.avutil_major,
                pool_size,
                self.pix_fmt_vaapi,
                self.pix_fmt_nv12,
                w as c_int,
                h as c_int,
            )
        };
        if rc != 0 {
            bail!("unsupported AVHWFramesContext ABI");
        }
        let rc = unsafe { (self.av_hwframe_ctx_init)(buf.ptr) };
        self.check("av_hwframe_ctx_init", rc)?;
        Ok(buf)
    }
}

pub(super) struct AvBufferRef {
    api: &'static FfmpegApi,
    ptr: *mut c_void,
}

unsafe impl Send for AvBufferRef {}

impl AvBufferRef {
    pub(super) fn clone_raw(&self) -> Result<*mut c_void> {
        let p = unsafe { (self.api.av_buffer_ref)(self.ptr) };
        if p.is_null() {
            bail!("av_buffer_ref returned null");
        }
        Ok(p)
    }
}

impl Drop for AvBufferRef {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { (self.api.av_buffer_unref)(&mut self.ptr) };
        }
    }
}

pub(super) struct AvCodecContext {
    api: &'static FfmpegApi,
    ptr: *mut c_void,
}

unsafe impl Send for AvCodecContext {}

impl AvCodecContext {
    pub(super) fn set_opt(&mut self, key: &CStr, val: &CStr, search_children: bool) -> Result<()> {
        let flags = if search_children {
            AV_OPT_SEARCH_CHILDREN
        } else {
            0
        };
        let rc = unsafe { (self.api.av_opt_set)(self.ptr, key.as_ptr(), val.as_ptr(), flags) };
        self.api
            .check(&format!("av_opt_set({})", key.to_string_lossy()), rc)
    }

    pub(super) fn set_opt_int(&mut self, key: &CStr, val: i64, search_children: bool) -> Result<()> {
        let flags = if search_children {
            AV_OPT_SEARCH_CHILDREN
        } else {
            0
        };
        let rc = unsafe { (self.api.av_opt_set_int)(self.ptr, key.as_ptr(), val, flags) };
        self.api
            .check(&format!("av_opt_set_int({})", key.to_string_lossy()), rc)
    }

    pub(super) fn has_opt(&self, key: &CStr, search_children: bool) -> bool {
        let flags = if search_children {
            AV_OPT_SEARCH_CHILDREN
        } else {
            0
        };
        let p =
            unsafe { (self.api.av_opt_find)(self.ptr, key.as_ptr(), std::ptr::null(), 0, flags) };
        !p.is_null()
    }

    pub(super) fn set_size(&mut self, w: u32, h: u32) -> Result<()> {
        let rc = unsafe {
            (self.api.av_opt_set_image_size)(
                self.ptr,
                c"video_size".as_ptr(),
                w as c_int,
                h as c_int,
                0,
            )
        };
        self.api.check("av_opt_set_image_size", rc)
    }

    pub(super) fn set_pix_fmt(&mut self, fmt: c_int) -> Result<()> {
        let rc = unsafe {
            (self.api.av_opt_set_pixel_fmt)(self.ptr, c"pixel_format".as_ptr(), fmt, 0)
        };
        self.api.check("av_opt_set_pixel_fmt", rc)
    }

    pub(super) fn set_framerate(&mut self, num: i32, den: i32) {
        unsafe { ah_av_ctx_set_framerate(self.ptr, self.api.framerate_offset, num, den) };
    }

    pub(super) fn set_hw_frames_ctx(&mut self, frames: &AvBufferRef) -> Result<()> {
        let cloned = frames.clone_raw()?;
        unsafe { ah_av_ctx_set_hw_frames_ctx(self.ptr, self.api.hw_frames_ctx_offset, cloned) };
        Ok(())
    }

    pub(super) fn open(&mut self, codec: *const c_void) -> Result<()> {
        let rc = unsafe { (self.api.avcodec_open2)(self.ptr, codec, std::ptr::null_mut()) };
        self.api.check("avcodec_open2", rc)
    }

    pub(super) fn send_frame(&mut self, frame: &AvFrame) -> Result<()> {
        let rc = unsafe { (self.api.avcodec_send_frame)(self.ptr, frame.ptr) };
        self.api.check("avcodec_send_frame", rc)
    }

    pub(super) fn receive_packet(&mut self, pkt: &mut AvPacket) -> Result<bool> {
        let rc = unsafe { (self.api.avcodec_receive_packet)(self.ptr, pkt.ptr) };
        if rc == AVERROR_EAGAIN {
            return Ok(false);
        }
        self.api.check("avcodec_receive_packet", rc)?;
        Ok(true)
    }
}

impl Drop for AvCodecContext {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { (self.api.avcodec_free_context)(&mut self.ptr) };
        }
    }
}

pub(super) struct AvFrame {
    api: &'static FfmpegApi,
    ptr: *mut c_void,
}

unsafe impl Send for AvFrame {}

impl AvFrame {
    pub(super) fn alloc_nv12(&mut self, w: u32, h: u32) -> Result<()> {
        let rc = unsafe {
            ah_av_frame_set_params(
                self.ptr,
                self.api.avutil_major,
                w as c_int,
                h as c_int,
                self.api.pix_fmt_nv12,
            )
        };
        if rc != 0 {
            bail!("unsupported AVFrame ABI");
        }
        let rc = unsafe { (self.api.av_frame_get_buffer)(self.ptr, 0) };
        self.api.check("av_frame_get_buffer", rc)
    }

    pub(super) fn make_writable(&mut self) -> Result<()> {
        let rc = unsafe { (self.api.av_frame_make_writable)(self.ptr) };
        self.api.check("av_frame_make_writable", rc)
    }

    pub(super) fn unref(&mut self) {
        unsafe { (self.api.av_frame_unref)(self.ptr) };
    }

    pub(super) fn get_hw_buffer(&mut self, frames: &AvBufferRef) -> Result<()> {
        self.unref();
        let rc = unsafe { (self.api.av_hwframe_get_buffer)(frames.ptr, self.ptr, 0) };
        self.api.check("av_hwframe_get_buffer", rc)
    }

    pub(super) fn upload_from(&mut self, sw: &AvFrame) -> Result<()> {
        let rc = unsafe { (self.api.av_hwframe_transfer_data)(self.ptr, sw.ptr, 0) };
        self.api.check("av_hwframe_transfer_data", rc)
    }

    pub(super) fn set_pict_type(&mut self, pict_type: c_int) {
        unsafe {
            let _ = ah_av_frame_set_pict_type(self.ptr, self.api.avutil_major, pict_type);
        }
    }

    pub(super) fn set_pts(&mut self, pts: i64) {
        unsafe {
            let _ = ah_av_frame_set_pts(self.ptr, self.api.avutil_major, pts);
        }
    }

    /// Mutable `(y_plane, y_stride, uv_plane, uv_stride)` for an allocated NV12 frame.
    pub(super) fn nv12_planes_mut(
        &mut self,
        w: u32,
        h: u32,
    ) -> Result<(&mut [u8], usize, &mut [u8], usize)> {
        let mut y_stride: c_int = 0;
        let mut uv_stride: c_int = 0;
        let y_ptr = unsafe { ah_av_frame_plane(self.ptr, self.api.avutil_major, 0, &mut y_stride) };
        let uv_ptr =
            unsafe { ah_av_frame_plane(self.ptr, self.api.avutil_major, 1, &mut uv_stride) };
        if y_ptr.is_null() || uv_ptr.is_null() {
            bail!("NV12 frame planes are null");
        }
        let y_stride = usize::try_from(y_stride).map_err(|_| anyhow!("negative Y linesize"))?;
        let uv_stride = usize::try_from(uv_stride).map_err(|_| anyhow!("negative UV linesize"))?;
        let (w, h) = (w as usize, h as usize);
        let uv_h = h.div_ceil(2);
        if y_stride < w || uv_stride < w {
            bail!("NV12 linesize ({y_stride}, {uv_stride}) smaller than width {w}");
        }
        let y_len = y_stride * h;
        let uv_len = uv_stride * uv_h;
        // Safety: planes 0 and 1 are disjoint buffers allocated by av_frame_get_buffer.
        let y = unsafe { std::slice::from_raw_parts_mut(y_ptr, y_len) };
        let uv = unsafe { std::slice::from_raw_parts_mut(uv_ptr, uv_len) };
        Ok((y, y_stride, uv, uv_stride))
    }
}

impl Drop for AvFrame {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { (self.api.av_frame_free)(&mut self.ptr) };
        }
    }
}

pub(super) struct AvPacket {
    api: &'static FfmpegApi,
    ptr: *mut c_void,
}

unsafe impl Send for AvPacket {}

impl AvPacket {
    pub(super) fn bytes(&self) -> &[u8] {
        let ptr = unsafe { ah_av_packet_data(self.ptr) };
        let size = unsafe { ah_av_packet_size(self.ptr) };
        if ptr.is_null() || size <= 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(ptr, size as usize) }
        }
    }

    pub(super) fn is_key(&self) -> bool {
        (unsafe { ah_av_packet_flags(self.ptr) } & AV_PKT_FLAG_KEY) != 0
    }

    pub(super) fn unref(&mut self) {
        unsafe { (self.api.av_packet_unref)(self.ptr) };
    }
}

impl Drop for AvPacket {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { (self.api.av_packet_free)(&mut self.ptr) };
        }
    }
}
