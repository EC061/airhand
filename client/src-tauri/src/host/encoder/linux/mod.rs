//! Linux 硬件视频编码（Intel / AMD VA-API 与 NVIDIA NVENC），支持 H.264 / HEVC / AV1。
//!
//! * 运行时通过 `dlopen` 加载系统 `libavcodec`（60–63，对应 FFmpeg 6–9）及其链接的
//!   `libavutil`（58–61），编译期不需要任何 FFmpeg `-dev` 头文件，运行时缺失时安静回退到 OpenH264；
//! * **VA-API**（`h264_vaapi` / `hevc_vaapi` / `av1_vaapi`）：Intel 核显/独显、AMD 显卡；
//! * **NVENC**（`h264_nvenc` / `hevc_nvenc` / `av1_nvenc`）：NVIDIA 显卡；
//! * **双显卡自动选择**（`hw_encoder = "auto"`）：优先选正在驱动已连接显示器的那一侧（例如 Intel 核显与
//!   NVIDIA 计算卡并存时默认用 Intel 核显，不唤醒也不占用 NVIDIA 显存；显示器接在 NVIDIA 上时优先 NVENC），
//!   仅在主后端缺少某种编码格式时才探测/回退到另一侧；
//! * **低延迟与拥塞友好**：零 B 帧、一进一出、超长 GOP、BT.709 有限范围、关键帧带完整参数集 / Sequence Header OBU；
//!   因为 FFmpeg 的 `*_vaapi`（需重建上下文）和 `*_nvenc`（`nvenc.c` 会强制 IDR）调整码率都会产生关键帧，
//!   只在已经需要关键帧、或者码率持续大幅下降（≥ 2 倍且持续 ≥ 3 秒）时才下发新码率，避免拥塞时触发 IDR 震荡。

mod ffmpeg;

use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use airhand_proto::session::{Codec, EncoderInfo};
use anyhow::{Context, Result, anyhow, bail};
use tracing::{debug, info, warn};
use yuvutils_rs::{YuvBiPlanarImageMut, YuvConversionMode, YuvRange, YuvStandardMatrix};

use self::ffmpeg::{
    AV_PICTURE_TYPE_I, AV_PICTURE_TYPE_NONE, AvBufferRef, AvCodecContext, AvFrame, AvPacket,
    FfmpegApi,
};
use super::{EncodedFrame, VideoEncoder, annexb_nal_types};
use crate::config::{self, HwEncoderPref};

/// NVIDIA PCI vendor ID（不用于 VA-API，打开其 renderD 节点可能唤醒休眠中的独显）
const PCI_VENDOR_NVIDIA: &str = "0x10de";
const PCI_VENDOR_INTEL: &str = "0x8086";
const PCI_VENDOR_AMD: &str = "0x1002";

/// VA-API 硬件帧池大小：4 帧已足够低延迟单帧编码（4K 下避免 20 帧占 ~250 MB 系统内存）
const VAAPI_POOL_SIZE: i32 = 4;

/// 码率下降（≥ 1.25 倍）持续这么久触发一次码率更新；上升（≥ 1.3 倍）持续更久才更新，避免拥塞时触发 IDR 震荡
const BITRATE_DROP_HOLD: Duration = Duration::from_secs(2);
const BITRATE_RAISE_HOLD: Duration = Duration::from_secs(6);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BitrateDir {
    Drop,
    Raise,
}

/// AV1 2 字节 Temporal Delimiter OBU（`obu_type=2`, `has_size=1`, `size=0`）
const AV1_TEMPORAL_DELIMITER: [u8; 2] = [0x12, 0x00];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    Vaapi,
    Nvenc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisplayOwner {
    Vaapi,
    Nvidia,
    Unknown,
}

#[derive(Debug, Clone)]
struct RenderNode {
    path: String,
    vendor_label: String,
    has_display: bool,
}

#[derive(Debug, Default)]
struct Capabilities {
    h264: bool,
    h265: bool,
    av1: bool,
}

impl Capabilities {
    fn has(&self, codec: Codec) -> bool {
        match codec {
            Codec::H264 => self.h264,
            Codec::H265 => self.h265,
            Codec::AV1 => self.av1,
            Codec::Unknown(_) => false,
        }
    }

    fn set(&mut self, codec: Codec) {
        match codec {
            Codec::H264 => self.h264 = true,
            Codec::H265 => self.h265 = true,
            Codec::AV1 => self.av1 = true,
            Codec::Unknown(_) => {}
        }
    }

    fn all_known(&self) -> bool {
        self.h264 && self.h265 && self.av1
    }
}

/// 纯函数：根据用户偏好与当前显示归属决定硬件后端尝试顺序。
pub(crate) fn backend_order(pref: HwEncoderPref, owner: DisplayOwner) -> Vec<Backend> {
    match pref {
        HwEncoderPref::Software => Vec::new(),
        HwEncoderPref::Vaapi => vec![Backend::Vaapi],
        HwEncoderPref::Nvenc => vec![Backend::Nvenc],
        HwEncoderPref::Auto => match owner {
            DisplayOwner::Nvidia => vec![Backend::Nvenc, Backend::Vaapi],
            DisplayOwner::Vaapi | DisplayOwner::Unknown => vec![Backend::Vaapi, Backend::Nvenc],
        },
    }
}

fn effective_pref() -> HwEncoderPref {
    if let Ok(raw) = std::env::var("AIRHAND_HW_ENCODER") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            if let Some(p) = HwEncoderPref::parse(trimmed) {
                return p;
            }
            warn!(
                value = trimmed,
                "ignoring unknown AIRHAND_HW_ENCODER value (expected auto/vaapi/nvenc/software)"
            );
        }
    }
    config::settings().hw_encoder
}

fn effective_vaapi_device() -> Option<String> {
    if let Ok(raw) = std::env::var("AIRHAND_VAAPI_DEVICE") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    config::settings().vaapi_device.clone()
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_ascii_lowercase())
}

fn vendor_label(vendor_hex: Option<&str>, sysfs_dir: &Path) -> String {
    match vendor_hex {
        Some(PCI_VENDOR_INTEL) => "Intel".into(),
        Some(PCI_VENDOR_AMD) => "AMD".into(),
        Some(PCI_VENDOR_NVIDIA) => "NVIDIA".into(),
        _ => std::fs::read_link(sysfs_dir.join("device/driver"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "VA-API".into()),
    }
}

/// 找出当前所有状态为 `connected` 的 DRM 显卡的 PCI 设备真实路径及其 vendor。
fn connected_drm_cards() -> Vec<(PathBuf, Option<String>)> {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    let mut connected_cards = std::collections::BTreeSet::<String>::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // 形如 `card0-HDMI-A-1`
        if let Some((card, _)) = name.split_once('-')
            && card.starts_with("card")
            && read_trimmed(&entry.path().join("status")).as_deref() == Some("connected")
        {
            connected_cards.insert(card.to_string());
        }
    }
    let mut out = Vec::new();
    for card in connected_cards {
        let dev_link = Path::new("/sys/class/drm").join(&card).join("device");
        let Ok(canon) = std::fs::canonicalize(&dev_link) else {
            continue;
        };
        let vendor = read_trimmed(&dev_link.join("vendor"));
        out.push((canon, vendor));
    }
    out
}

/// 通过 DRM sysfs 与 X11 RandR 判断当前已连接显示器归属哪一侧显卡（进程内缓存一次）。
fn display_owner() -> DisplayOwner {
    static OWNER: OnceLock<DisplayOwner> = OnceLock::new();
    *OWNER.get_or_init(detect_display_owner)
}

fn detect_display_owner() -> DisplayOwner {
    let cards = connected_drm_cards();
    if !cards.is_empty() {
        let any_non_nv = cards
            .iter()
            .any(|(_, v)| v.as_deref() != Some(PCI_VENDOR_NVIDIA));
        if any_non_nv {
            return DisplayOwner::Vaapi;
        }
        return DisplayOwner::Nvidia;
    }
    // 未开启 nvidia-drm.modeset=1 时 sysfs 可能没有 connected 连接器：再查一次 X11 RandR provider
    if let Some(owner) = detect_display_owner_x11() {
        return owner;
    }
    DisplayOwner::Unknown
}

fn detect_display_owner_x11() -> Option<DisplayOwner> {
    use x11rb::protocol::randr::ConnectionExt as _;
    let x = crate::host::x11::X11::connect().ok()?;
    if !x.has_extension(x11rb::protocol::randr::X11_EXTENSION_NAME) {
        return None;
    }
    let providers = x.conn.randr_get_providers(x.root()).ok()?.reply().ok()?;
    let mut has_nvidia = false;
    let mut has_other = false;
    for provider in providers.providers {
        if let Ok(cookie) = x.conn.randr_get_provider_info(provider, providers.timestamp)
            && let Ok(info) = cookie.reply()
        {
            // 忽略没有物理输出的 offload sink（例如双显卡上未接显示器的 modesetting provider）
            if info.outputs.is_empty() {
                continue;
            }
            let name = String::from_utf8_lossy(&info.name).to_ascii_lowercase();
            if name.contains("nvidia") {
                has_nvidia = true;
            } else if !name.is_empty() {
                has_other = true;
            }
        }
    }
    if has_nvidia && !has_other {
        Some(DisplayOwner::Nvidia)
    } else if has_other {
        Some(DisplayOwner::Vaapi)
    } else {
        None
    }
}

/// 枚举适合 VA-API 编码的 DRM render 节点（先查 sysfs vendor 跳过 NVIDIA，不打开独显设备节点）。
fn discover_vaapi_nodes(device_override: Option<&str>) -> Vec<RenderNode> {
    if let Some(path) = device_override {
        let name = Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let sysfs = Path::new("/sys/class/drm").join(&name);
        let vendor = read_trimmed(&sysfs.join("device/vendor"));
        return vec![RenderNode {
            path: path.to_string(),
            vendor_label: vendor_label(vendor.as_deref(), &sysfs),
            has_display: true,
        }];
    }

    let connected = connected_drm_cards();
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    let mut nodes = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("renderD") {
            continue;
        }
        let sysfs = entry.path();
        let vendor = read_trimmed(&sysfs.join("device/vendor"));
        if vendor.as_deref() == Some(PCI_VENDOR_NVIDIA) {
            debug!(node = %name, "skipping NVIDIA DRM render node for VA-API");
            continue;
        }
        let dev_path = format!("/dev/dri/{name}");
        if !Path::new(&dev_path).exists() {
            continue;
        }
        let canon = std::fs::canonicalize(sysfs.join("device")).ok();
        let has_display = canon
            .as_ref()
            .is_some_and(|c| connected.iter().any(|(cc, _)| cc == c));
        nodes.push(RenderNode {
            path: dev_path,
            vendor_label: vendor_label(vendor.as_deref(), &sysfs),
            has_display,
        });
    }
    // 驱动已连接显示器的节点优先，其次按设备路径排序
    nodes.sort_by(|a, b| b.has_display.cmp(&a.has_display).then(a.path.cmp(&b.path)));
    nodes
}

fn vaapi_encoder_name(codec: Codec) -> Option<&'static CStr> {
    match codec {
        Codec::H264 => Some(c"h264_vaapi"),
        Codec::H265 => Some(c"hevc_vaapi"),
        Codec::AV1 => Some(c"av1_vaapi"),
        Codec::Unknown(_) => None,
    }
}

fn nvenc_encoder_name(codec: Codec) -> Option<&'static CStr> {
    match codec {
        Codec::H264 => Some(c"h264_nvenc"),
        Codec::H265 => Some(c"hevc_nvenc"),
        Codec::AV1 => Some(c"av1_nvenc"),
        Codec::Unknown(_) => None,
    }
}

fn codec_label(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 | Codec::Unknown(_) => "H.264",
        Codec::H265 => "HEVC",
        Codec::AV1 => "AV1",
    }
}

fn vbv_bufsize(bps: i64, fps: u8) -> i64 {
    let per_frame = bps / (fps.max(1) as i64);
    per_frame.saturating_mul(3).clamp(bps / 20, bps).max(16_384)
}

fn apply_common_options(
    ctx: &mut AvCodecContext,
    w: u32,
    h: u32,
    fps: u8,
    kbps: u32,
    pix_fmt: i32,
) -> Result<()> {
    let fps = fps.max(1);
    let bps = (kbps.max(100) as i64).saturating_mul(1000);
    let maxrate = bps.saturating_mul(3) / 2;
    let bufsize = vbv_bufsize(bps, fps);
    let tb = CString::new(format!("1/{fps}"))?;

    ctx.set_size(w, h)?;
    ctx.set_pix_fmt(pix_fmt)?;
    ctx.set_opt(c"time_base", &tb, false)?;
    ctx.set_framerate(fps as i32, 1);
    ctx.set_opt_int(c"b", bps, false)?;
    ctx.set_opt_int(c"maxrate", maxrate, false)?;
    ctx.set_opt_int(c"bufsize", bufsize, false)?;
    ctx.set_opt_int(c"g", (fps as i64) * 600, false)?;
    ctx.set_opt_int(c"bf", 0, false)?;
    let _ = ctx.set_opt(c"color_range", c"tv", false);
    let _ = ctx.set_opt(c"color_primaries", c"bt709", false);
    let _ = ctx.set_opt(c"color_trc", c"bt709", false);
    let _ = ctx.set_opt(c"colorspace", c"bt709", false);
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct VaapiProfile {
    low_power: bool,
    use_vbr: bool,
}

fn open_vaapi_ctx(
    api: &'static FfmpegApi,
    codec: Codec,
    (w, h): (u32, u32),
    fps: u8,
    kbps: u32,
    hw_frames: &AvBufferRef,
    prefer: Option<VaapiProfile>,
) -> Result<(AvCodecContext, VaapiProfile)> {
    let enc_name = vaapi_encoder_name(codec).ok_or_else(|| anyhow!("unsupported codec {codec:?}"))?;
    let ff_codec = api
        .find_encoder(enc_name)
        .ok_or_else(|| anyhow!("FFmpeg lacks {}", enc_name.to_string_lossy()))?;

    let lp_order: [bool; 2] = match prefer {
        Some(p) if p.low_power => [true, false],
        _ => [false, true],
    };
    let vbr_order: [bool; 2] = match prefer {
        Some(p) if !p.use_vbr => [false, true],
        _ => [true, false],
    };

    let mut last_err = anyhow!("failed to open {}", enc_name.to_string_lossy());
    for low_power in lp_order {
        // 先试 VBR（避免 Intel iHD 在 CBR 下往静止帧塞满 filler 字节）；少数老驱动不支持时再试默认 rc_mode
        for use_vbr in vbr_order {
            let mut ctx = api.alloc_context(ff_codec)?;
            apply_common_options(&mut ctx, w, h, fps, kbps, api.pix_fmt_vaapi)?;
            let _ = ctx.set_opt_int(c"async_depth", 1, true);
            let _ = ctx.set_opt_int(c"idr_interval", 0, true);
            if use_vbr {
                let _ = ctx.set_opt(c"rc_mode", c"VBR", true);
            }
            if ctx.has_opt(c"low_power", true) {
                let _ = ctx.set_opt_int(c"low_power", i64::from(low_power), true);
            }
            match codec {
                Codec::H264 => {
                    let _ = ctx.set_opt(c"profile", c"high", true);
                }
                Codec::H265 => {
                    let _ = ctx.set_opt(c"profile", c"main", true);
                }
                Codec::AV1 | Codec::Unknown(_) => {}
            }
            ctx.set_hw_frames_ctx(hw_frames)?;
            match ctx.open(ff_codec) {
                Ok(()) => return Ok((ctx, VaapiProfile { low_power, use_vbr })),
                Err(e) => {
                    debug!(
                        encoder = %enc_name.to_string_lossy(),
                        low_power,
                        use_vbr,
                        "vaapi open attempt failed: {e:#}"
                    );
                    last_err = e;
                }
            }
        }
    }
    Err(last_err)
}

fn open_nvenc_ctx(
    api: &'static FfmpegApi,
    codec: Codec,
    w: u32,
    h: u32,
    fps: u8,
    kbps: u32,
) -> Result<AvCodecContext> {
    let enc_name = nvenc_encoder_name(codec).ok_or_else(|| anyhow!("unsupported codec {codec:?}"))?;
    let ff_codec = api
        .find_encoder(enc_name)
        .ok_or_else(|| anyhow!("FFmpeg lacks {}", enc_name.to_string_lossy()))?;

    let mut ctx = api.alloc_context(ff_codec)?;
    apply_common_options(&mut ctx, w, h, fps, kbps, api.pix_fmt_nv12)?;
    let _ = ctx.set_opt(c"preset", c"p4", true);
    let _ = ctx.set_opt(c"tune", c"ull", true);
    let _ = ctx.set_opt(c"rc", c"vbr", true);
    let _ = ctx.set_opt_int(c"zerolatency", 1, true);
    let _ = ctx.set_opt_int(c"delay", 0, true);
    let _ = ctx.set_opt_int(c"forced-idr", 1, true);
    match codec {
        Codec::H264 => {
            let _ = ctx.set_opt(c"profile", c"high", true);
        }
        Codec::H265 => {
            let _ = ctx.set_opt(c"profile", c"main", true);
        }
        Codec::AV1 | Codec::Unknown(_) => {}
    }
    ctx.open(ff_codec)?;
    Ok(ctx)
}

/// 探测单个 VA-API 节点支持哪些编码格式（复用同一个硬件设备与帧池，避免反复开关设备）。
fn probe_vaapi_codecs(
    api: &'static FfmpegApi,
    device_override: Option<&str>,
    missing: &[Codec],
    caps: &mut Capabilities,
) {
    for node in discover_vaapi_nodes(device_override) {
        let Ok(hw_dev) = api.create_vaapi_device(&node.path) else {
            debug!(device = %node.path, "VA-API device unavailable");
            continue;
        };
        let Ok(hw_frames) = api.create_vaapi_frames(&hw_dev, 256, 256, VAAPI_POOL_SIZE) else {
            debug!(device = %node.path, "VA-API hwframe init failed");
            continue;
        };
        for &codec in missing {
            if caps.has(codec) {
                continue;
            }
            if open_vaapi_ctx(api, codec, (256, 256), 30, 2000, &hw_frames, None).is_ok() {
                info!(device = %node.path, vendor = %node.vendor_label, ?codec, "VA-API hardware encoder available");
                caps.set(codec);
            }
        }
        if caps.all_known() {
            break;
        }
    }
}

/// 探测 NVENC 支持哪些编码格式（逐个探测并立即释放上下文，最多只占 1 个会话，探测完显存归零）。
fn probe_nvenc_codecs(api: &'static FfmpegApi, missing: &[Codec], caps: &mut Capabilities) {
    for &codec in missing {
        if caps.has(codec) {
            continue;
        }
        if open_nvenc_ctx(api, codec, 256, 256, 30, 2000).is_ok() {
            info!(?codec, "NVENC hardware encoder available");
            caps.set(codec);
        }
    }
}

fn capabilities() -> &'static Capabilities {
    static CAPS: OnceLock<Capabilities> = OnceLock::new();
    CAPS.get_or_init(|| {
        let mut caps = Capabilities::default();
        let pref = effective_pref();
        let owner = display_owner();
        let order = backend_order(pref, owner);
        info!(?pref, ?owner, ?order, "probing Linux hardware encoders");
        if order.is_empty() {
            return caps;
        }
        let Some(api) = ffmpeg::api() else {
            return caps;
        };
        let device_override = effective_vaapi_device();
        for backend in order {
            let missing: Vec<Codec> = Codec::KNOWN
                .iter()
                .copied()
                .filter(|&c| !caps.has(c))
                .collect();
            if missing.is_empty() {
                break;
            }
            match backend {
                Backend::Vaapi => {
                    probe_vaapi_codecs(api, device_override.as_deref(), &missing, &mut caps)
                }
                Backend::Nvenc => probe_nvenc_codecs(api, &missing, &mut caps),
            }
        }
        caps
    })
}

pub fn has_hardware(codec: Codec) -> bool {
    capabilities().has(codec)
}

/// 后台预热硬件编码器探测，避免首次远程连接等待探测完成。
pub fn warm_up() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let _ = std::thread::Builder::new()
            .name("airhand-hw-probe".into())
            .spawn(|| {
                let _ = super::available_codecs();
            });
    });
}

// ───────────────────────────── 码流检查与打包 ─────────────────────────────

/// AV1 OBU 码流中是否包含 Sequence Header OBU（`obu_type == 1`）。
pub(crate) fn av1_has_sequence_header(data: &[u8]) -> bool {
    let mut i = 0usize;
    while i < data.len() {
        let hdr = data[i];
        i += 1;
        let obu_type = (hdr >> 3) & 0x0f;
        let has_ext = (hdr >> 2) & 1 != 0;
        let has_size = (hdr >> 1) & 1 != 0;
        if obu_type == 1 {
            return true;
        }
        if has_ext {
            if i >= data.len() {
                break;
            }
            i += 1;
        }
        if !has_size {
            break;
        }
        let mut size = 0usize;
        let mut shift = 0u32;
        let mut done = false;
        while i < data.len() && shift < 56 {
            let b = data[i];
            i += 1;
            size |= ((b & 0x7f) as usize) << shift;
            if b & 0x80 == 0 {
                done = true;
                break;
            }
            shift += 7;
        }
        if !done || size > data.len().saturating_sub(i) {
            break;
        }
        i += size;
    }
    false
}

/// 确保 AV1 帧以 Temporal Delimiter OBU（`0x12 0x00`）开头，满足 WebCodecs / Android / iOS / ffmpeg OBU 解析要求。
pub(crate) fn ensure_av1_temporal_delimiter(src: &[u8]) -> Vec<u8> {
    if !src.is_empty() && ((src[0] >> 3) & 0x0f) == 2 {
        return src.to_vec();
    }
    let mut out = Vec::with_capacity(AV1_TEMPORAL_DELIMITER.len() + src.len());
    out.extend_from_slice(&AV1_TEMPORAL_DELIMITER);
    out.extend_from_slice(src);
    out
}

fn detect_keyframe(codec: Codec, pkt_key: bool, data: &[u8]) -> bool {
    match codec {
        Codec::H264 => pkt_key || annexb_nal_types(data, false).any(|t| t == 5),
        Codec::H265 => pkt_key || annexb_nal_types(data, true).any(|t| (16..=21).contains(&t)),
        Codec::AV1 => pkt_key || av1_has_sequence_header(data),
        Codec::Unknown(_) => pkt_key,
    }
}

fn keyframe_has_headers(codec: Codec, data: &[u8]) -> bool {
    match codec {
        Codec::H264 => {
            let mut sps = false;
            let mut pps = false;
            for t in annexb_nal_types(data, false) {
                sps |= t == 7;
                pps |= t == 8;
            }
            sps && pps
        }
        Codec::H265 => {
            let mut vps = false;
            let mut sps = false;
            let mut pps = false;
            for t in annexb_nal_types(data, true) {
                vps |= t == 32;
                sps |= t == 33;
                pps |= t == 34;
            }
            vps && sps && pps
        }
        Codec::AV1 => av1_has_sequence_header(data),
        Codec::Unknown(_) => true,
    }
}

// ───────────────────────────── 编码器实现 ─────────────────────────────

enum Mode {
    Vaapi {
        _hw_device: AvBufferRef,
        hw_frames: AvBufferRef,
        hw_frame: AvFrame,
        profile: VaapiProfile,
    },
    Nvenc,
}

pub struct LinuxHwEncoder {
    api: &'static FfmpegApi,
    ctx: AvCodecContext,
    sw_frame: AvFrame,
    pkt: AvPacket,
    mode: Mode,
    info: EncoderInfo,
    fps: u8,
    active_kbps: u32,
    target_kbps: u32,
    pending_bitrate: Option<(Instant, BitrateDir)>,
    pts: i64,
    /// `av_frame_get_buffer` 的 UV 行步长大于宽度时的紧凑暂存缓冲（见 `vt.rs`）
    uv_scratch: Vec<u8>,
}

impl LinuxHwEncoder {
    /// 按当前配置与显示归属的后端顺序尝试为 `codec` 创建硬件编码器（先试主后端，再试备选后端）。
    pub fn new(codec: Codec, w: u32, h: u32, fps: u8, kbps: u32) -> Result<Self> {
        let pref = effective_pref();
        let owner = display_owner();
        let order = backend_order(pref, owner);
        if order.is_empty() {
            bail!("hardware encoding disabled by configuration");
        }
        let device_override = effective_vaapi_device();
        let mut last_err = anyhow!("no Linux hardware encoder available for {codec:?}");
        for backend in order {
            let res = match backend {
                Backend::Vaapi => Self::new_vaapi(codec, w, h, fps, kbps, device_override.as_deref()),
                Backend::Nvenc => Self::new_nvenc(codec, w, h, fps, kbps),
            };
            match res {
                Ok(enc) => return Ok(enc),
                Err(e) => {
                    debug!(?backend, ?codec, "Linux hardware encoder attempt failed: {e:#}");
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    pub(crate) fn new_vaapi(
        codec: Codec,
        w: u32,
        h: u32,
        fps: u8,
        kbps: u32,
        device_override: Option<&str>,
    ) -> Result<Self> {
        let api = ffmpeg::api().ok_or_else(|| anyhow!("FFmpeg libavcodec not available"))?;
        let nodes = discover_vaapi_nodes(device_override);
        if nodes.is_empty() {
            bail!("no VA-API DRM render node found");
        }
        let mut last_err = anyhow!("no VA-API device accepted {codec:?}");
        for node in nodes {
            match Self::open_vaapi_node(api, &node, codec, w, h, fps, kbps) {
                Ok(enc) => return Ok(enc),
                Err(e) => {
                    debug!(device = %node.path, ?codec, "VA-API open failed: {e:#}");
                    last_err = e.context(node.path);
                }
            }
        }
        Err(last_err)
    }

    fn open_vaapi_node(
        api: &'static FfmpegApi,
        node: &RenderNode,
        codec: Codec,
        w: u32,
        h: u32,
        fps: u8,
        kbps: u32,
    ) -> Result<Self> {
        let hw_device = api.create_vaapi_device(&node.path)?;
        let hw_frames = api.create_vaapi_frames(&hw_device, w, h, VAAPI_POOL_SIZE)?;
        let (ctx, profile) = open_vaapi_ctx(api, codec, (w, h), fps, kbps, &hw_frames, None)?;
        let mut sw_frame = api.alloc_frame()?;
        sw_frame.alloc_nv12(w, h)?;
        let hw_frame = api.alloc_frame()?;
        let pkt = api.alloc_packet()?;

        let mut enc = Self {
            api,
            ctx,
            sw_frame,
            pkt,
            mode: Mode::Vaapi {
                _hw_device: hw_device,
                hw_frames,
                hw_frame,
                profile,
            },
            info: EncoderInfo {
                codec,
                name: format!("VA-API {} ({})", codec_label(codec), node.vendor_label),
                hardware: true,
                width: w,
                height: h,
                stream: 0,
            },
            fps: fps.max(1),
            active_kbps: kbps.max(100),
            target_kbps: kbps.max(100),
            pending_bitrate: None,
            pts: 0,
            uv_scratch: Vec::new(),
        };
        enc.smoke_test().context("VA-API smoke test")?;
        Ok(enc)
    }

    pub(crate) fn new_nvenc(
        codec: Codec,
        w: u32,
        h: u32,
        fps: u8,
        kbps: u32,
    ) -> Result<Self> {
        let api = ffmpeg::api().ok_or_else(|| anyhow!("FFmpeg libavcodec not available"))?;
        let ctx = open_nvenc_ctx(api, codec, w, h, fps, kbps)?;
        let mut sw_frame = api.alloc_frame()?;
        sw_frame.alloc_nv12(w, h)?;
        let pkt = api.alloc_packet()?;

        let mut enc = Self {
            api,
            ctx,
            sw_frame,
            pkt,
            mode: Mode::Nvenc,
            info: EncoderInfo {
                codec,
                name: format!("NVENC {}", codec_label(codec)),
                hardware: true,
                width: w,
                height: h,
                stream: 0,
            },
            fps: fps.max(1),
            active_kbps: kbps.max(100),
            target_kbps: kbps.max(100),
            pending_bitrate: None,
            pts: 0,
            uv_scratch: Vec::new(),
        };
        enc.smoke_test().context("NVENC smoke test")?;
        Ok(enc)
    }

    /// 冒烟测试：编码一帧全黑关键帧并完整取走丢弃，确保硬件在当前分辨率下可用且码流带完整参数集。
    fn smoke_test(&mut self) -> Result<()> {
        let black = vec![0u8; (self.info.width * self.info.height * 4) as usize];
        let frame = self
            .encode_raw(&black, true)?
            .ok_or_else(|| anyhow!("encoder produced no output for the test keyframe"))?;
        if frame.data.is_empty() {
            bail!("encoder produced an empty test keyframe");
        }
        if !frame.key || !keyframe_has_headers(self.info.codec, &frame.data) {
            bail!(
                "encoder test keyframe is missing required in-band headers for {:?}",
                self.info.codec
            );
        }
        Ok(())
    }

    fn apply_bitrate(&mut self, kbps: u32) -> Result<bool> {
        let kbps = kbps.max(100);
        let bps = (kbps as i64).saturating_mul(1000);
        let maxrate = bps.saturating_mul(3) / 2;
        let bufsize = vbv_bufsize(bps, self.fps);
        match &self.mode {
            Mode::Nvenc => {
                self.ctx.set_opt_int(c"b", bps, false)?;
                self.ctx.set_opt_int(c"maxrate", maxrate, false)?;
                self.ctx.set_opt_int(c"bufsize", bufsize, false)?;
                self.active_kbps = kbps;
                self.pending_bitrate = None;
                Ok(false)
            }
            Mode::Vaapi {
                hw_frames, profile, ..
            } => {
                let (new_ctx, new_profile) = open_vaapi_ctx(
                    self.api,
                    self.info.codec,
                    (self.info.width, self.info.height),
                    self.fps,
                    kbps,
                    hw_frames,
                    Some(*profile),
                )?;
                self.ctx = new_ctx;
                if let Mode::Vaapi { profile: p, .. } = &mut self.mode {
                    *p = new_profile;
                }
                self.active_kbps = kbps;
                self.pending_bitrate = None;
                Ok(true)
            }
        }
    }

    fn write_bgra_to_nv12(&mut self, bgra: &[u8]) -> Result<()> {
        let (w, h) = (self.info.width, self.info.height);
        let expected = (w as usize) * (h as usize) * 4;
        if bgra.len() < expected {
            bail!("BGRA buffer too small: {} < {expected}", bgra.len());
        }
        // 确保底层缓冲没有被 FFmpeg 内部持有引用（零开销快速路径）
        self.sw_frame.make_writable()?;
        let (y, y_stride, uv, uv_stride) = self.sw_frame.nv12_planes_mut(w, h)?;
        let uv_row = w as usize;
        let uv_h = (h as usize).div_ceil(2);
        if uv_stride == uv_row {
            let mut img = YuvBiPlanarImageMut {
                y_plane: yuvutils_rs::BufferStoreMut::Borrowed(y),
                y_stride: y_stride as u32,
                uv_plane: yuvutils_rs::BufferStoreMut::Borrowed(uv),
                uv_stride: uv_row as u32,
                width: w,
                height: h,
            };
            yuvutils_rs::bgra_to_yuv_nv12(
                &mut img,
                &bgra[..expected],
                w * 4,
                YuvRange::Limited,
                YuvStandardMatrix::Bt709,
                YuvConversionMode::Balanced,
            )
            .map_err(|e| anyhow!("color conversion failed: {e:?}"))?;
        } else {
            // yuvutils 0.8 的 NV12 转换要求 UV 紧密排列（见 vt.rs / capture/mod.rs），
            // 非 16 对齐宽度（如 1366、1590、1728）时先写进紧凑暂存缓冲再按行拷进对齐后的平面
            self.uv_scratch.resize(uv_row * uv_h, 128);
            let mut img = YuvBiPlanarImageMut {
                y_plane: yuvutils_rs::BufferStoreMut::Borrowed(y),
                y_stride: y_stride as u32,
                uv_plane: yuvutils_rs::BufferStoreMut::Borrowed(&mut self.uv_scratch),
                uv_stride: uv_row as u32,
                width: w,
                height: h,
            };
            yuvutils_rs::bgra_to_yuv_nv12(
                &mut img,
                &bgra[..expected],
                w * 4,
                YuvRange::Limited,
                YuvStandardMatrix::Bt709,
                YuvConversionMode::Balanced,
            )
            .map_err(|e| anyhow!("color conversion failed: {e:?}"))?;
            for (dst, src) in uv
                .chunks_mut(uv_stride)
                .zip(self.uv_scratch.chunks(uv_row))
                .take(uv_h)
            {
                dst[..uv_row].copy_from_slice(src);
            }
        }
        Ok(())
    }

    fn encode_raw(&mut self, bgra: &[u8], force_key: bool) -> Result<Option<EncodedFrame>> {
        self.write_bgra_to_nv12(bgra)?;
        let pts = self.pts;
        self.pts += 1;
        let pict_type = if force_key {
            AV_PICTURE_TYPE_I
        } else {
            AV_PICTURE_TYPE_NONE
        };

        match &mut self.mode {
            Mode::Vaapi {
                hw_frames,
                hw_frame,
                ..
            } => {
                hw_frame.get_hw_buffer(hw_frames)?;
                hw_frame.upload_from(&self.sw_frame)?;
                hw_frame.set_pts(pts);
                hw_frame.set_pict_type(pict_type);
                self.ctx.send_frame(hw_frame)?;
            }
            Mode::Nvenc => {
                self.sw_frame.set_pts(pts);
                self.sw_frame.set_pict_type(pict_type);
                self.ctx.send_frame(&self.sw_frame)?;
            }
        }

        // 低延迟一进一出：循环取完所有就绪包（通常恰好 1 个），不残留堆积；若意外返回 EAGAIN 报错重建，
        // 避免从此多出永久 1 帧延迟和 key 标志错位
        let mut latest: Option<(Vec<u8>, bool)> = None;
        while self.ctx.receive_packet(&mut self.pkt)? {
            let raw = self.pkt.bytes();
            let pkt_key = self.pkt.is_key();
            if !raw.is_empty() {
                let data = if self.info.codec == Codec::AV1 {
                    ensure_av1_temporal_delimiter(raw)
                } else {
                    raw.to_vec()
                };
                latest = Some((data, pkt_key));
            }
            self.pkt.unref();
        }
        let Some((data, pkt_key)) = latest else {
            bail!("hardware encoder buffered frame instead of producing immediate output");
        };
        let key = detect_keyframe(self.info.codec, pkt_key, &data);
        Ok(Some(EncodedFrame {
            data,
            key,
            ltr: None,
        }))
    }
}

impl VideoEncoder for LinuxHwEncoder {
    fn info(&self) -> &EncoderInfo {
        &self.info
    }

    fn set_bitrate(&mut self, kbps: u32) {
        let kbps = kbps.max(100);
        self.target_kbps = kbps;
        let dir = if kbps.saturating_mul(5) <= self.active_kbps.saturating_mul(4) {
            Some(BitrateDir::Drop)
        } else if kbps.saturating_mul(10) >= self.active_kbps.saturating_mul(13) {
            Some(BitrateDir::Raise)
        } else {
            None
        };
        match dir {
            Some(d) => match self.pending_bitrate {
                Some((_, cur)) if cur == d => {}
                _ => self.pending_bitrate = Some((Instant::now(), d)),
            },
            None => self.pending_bitrate = None,
        }
    }

    fn encode(&mut self, bgra: &[u8], mut force_key: bool) -> Result<Option<EncodedFrame>> {
        let sustained_change = self.pending_bitrate.is_some_and(|(since, dir)| {
            let hold = match dir {
                BitrateDir::Drop => BITRATE_DROP_HOLD,
                BitrateDir::Raise => BITRATE_RAISE_HOLD,
            };
            since.elapsed() >= hold
        });
        if self.target_kbps != self.active_kbps && (force_key || sustained_change) {
            let target = self.target_kbps;
            match self.apply_bitrate(target) {
                Ok(reopened) => {
                    if reopened {
                        force_key = true;
                    }
                    debug!(
                        encoder = %self.info.name,
                        kbps = target,
                        reopened,
                        "applied hardware encoder bitrate update"
                    );
                }
                Err(e) => {
                    warn!(
                        encoder = %self.info.name,
                        kbps = target,
                        "bitrate update failed, keeping current rate: {e:#}"
                    );
                    self.pending_bitrate = None;
                }
            }
        }
        self.encode_raw(bgra, force_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_order_rules() {
        assert_eq!(
            backend_order(HwEncoderPref::Auto, DisplayOwner::Vaapi),
            vec![Backend::Vaapi, Backend::Nvenc]
        );
        assert_eq!(
            backend_order(HwEncoderPref::Auto, DisplayOwner::Unknown),
            vec![Backend::Vaapi, Backend::Nvenc]
        );
        assert_eq!(
            backend_order(HwEncoderPref::Auto, DisplayOwner::Nvidia),
            vec![Backend::Nvenc, Backend::Vaapi]
        );
        assert_eq!(
            backend_order(HwEncoderPref::Vaapi, DisplayOwner::Nvidia),
            vec![Backend::Vaapi]
        );
        assert_eq!(
            backend_order(HwEncoderPref::Nvenc, DisplayOwner::Vaapi),
            vec![Backend::Nvenc]
        );
        assert!(backend_order(HwEncoderPref::Software, DisplayOwner::Vaapi).is_empty());
    }

    #[test]
    fn av1_obu_helpers() {
        // TD(2, len=0) + SeqHdr(1, len=2)
        let with_td = [0x12, 0x00, 0x0a, 0x02, 0xaa, 0xbb];
        assert!(av1_has_sequence_header(&with_td));
        assert_eq!(ensure_av1_temporal_delimiter(&with_td), with_td);

        // SeqHdr(1, len=2) without TD -> TD prepended
        let no_td = [0x0a, 0x02, 0xaa, 0xbb];
        assert!(av1_has_sequence_header(&no_td));
        assert_eq!(ensure_av1_temporal_delimiter(&no_td), with_td);

        // FrameHdr(3, len=2) only -> no sequence header
        let delta = [0x1a, 0x02, 0x01, 0x02];
        assert!(!av1_has_sequence_header(&delta));
        assert_eq!(
            ensure_av1_temporal_delimiter(&delta),
            [0x12, 0x00, 0x1a, 0x02, 0x01, 0x02]
        );
    }

    /// 构造带运动图案的 BGRA 测试帧
    fn pattern_frame(w: u32, h: u32, idx: u32) -> Vec<u8> {
        let mut px = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                px[i] = ((x + idx * 17) % 256) as u8;
                px[i + 1] = ((y + idx * 9) % 256) as u8;
                px[i + 2] = (((x / 16 + y / 16 + idx) % 2) * 255) as u8;
                px[i + 3] = 255;
            }
        }
        px
    }

    /// 左半纯红、右半纯蓝（BGRA）
    fn red_blue_frame(w: u32, h: u32) -> Vec<u8> {
        let mut px = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                if x < w / 2 {
                    // Red in BGRA
                    px[i..i + 4].copy_from_slice(&[0, 0, 255, 255]);
                } else {
                    // Blue in BGRA
                    px[i..i + 4].copy_from_slice(&[255, 0, 0, 255]);
                }
            }
        }
        px
    }

    fn ffmpeg_format(codec: Codec) -> &'static str {
        match codec {
            Codec::H265 => "hevc",
            Codec::AV1 => "obu",
            Codec::H264 | Codec::Unknown(_) => "h264",
        }
    }

    /// 用系统 ffmpeg 严格模式解码全部帧并返回每帧 MD5（系统无 ffmpeg 时跳过）
    fn ffmpeg_strict_decode_count(codec: Codec, stream: &[u8], tag: &str) -> Option<usize> {
        let path = std::env::temp_dir().join(format!(
            "airhand-linux-hw-{}-{tag}-{}.bin",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::write(&path, stream).unwrap();
        let out = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-err_detect",
                "explode",
                "-f",
                ffmpeg_format(codec),
                "-i",
            ])
            .arg(&path)
            .args(["-f", "framemd5", "-"])
            .output();
        let _ = std::fs::remove_file(&path);
        let out = out.ok()?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "ffmpeg ({tag}): {stderr}");
        assert!(stderr.is_empty(), "ffmpeg stderr ({tag}): {stderr}");
        let stdout = String::from_utf8(out.stdout).unwrap();
        let frames = stdout.lines().filter(|l| !l.starts_with('#')).count();
        Some(frames)
    }

    /// 用系统 ffmpeg 解码单帧到 RGB24 并校验左红右蓝的 BT.709 色彩准确性
    fn verify_red_blue_roundtrip(enc: &mut LinuxHwEncoder, tag: &str) {
        let (w, h) = (enc.info.width, enc.info.height);
        let src = red_blue_frame(w, h);
        let ef = enc
            .encode(&src, true)
            .unwrap()
            .expect("red/blue keyframe output");
        assert!(ef.key, "{tag}: expected keyframe");
        assert!(
            keyframe_has_headers(enc.info.codec, &ef.data),
            "{tag}: missing keyframe headers"
        );

        let path = std::env::temp_dir().join(format!(
            "airhand-linux-color-{}-{tag}-{}.bin",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::write(&path, &ef.data).unwrap();
        let out = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                ffmpeg_format(enc.info.codec),
                "-i",
            ])
            .arg(&path)
            .args([
                "-frames:v",
                "1",
                "-pix_fmt",
                "rgb24",
                "-f",
                "rawvideo",
                "-",
            ])
            .output();
        let _ = std::fs::remove_file(&path);
        let Ok(out) = out else {
            return;
        };
        assert!(
            out.status.success(),
            "ffmpeg color decode ({tag}): {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let rgb = out.stdout;
        assert_eq!(rgb.len(), (w * h * 3) as usize);
        let px = |x: u32, y: u32| {
            let i = ((y * w + x) * 3) as usize;
            [rgb[i], rgb[i + 1], rgb[i + 2]]
        };
        let left = px(w / 4, h / 2);
        let right = px(w * 3 / 4, h / 2);
        assert!(
            left[0] > 220 && left[1] < 35 && left[2] < 35,
            "{tag}: left pixel should be red, got {left:?}"
        );
        assert!(
            right[0] < 35 && right[1] < 35 && right[2] > 220,
            "{tag}: right pixel should be blue, got {right:?}"
        );
    }

    fn exercise_encoder(mut enc: LinuxHwEncoder, tag: &str) {
        let (w, h) = (enc.info.width, enc.info.height);
        let codec = enc.info.codec;
        let mut stream = Vec::new();
        for i in 0..10u32 {
            let force_key = i == 0 || i == 5;
            if i == 5 {
                // 强制关键帧时应顺带应用新的码率
                enc.set_bitrate(2000);
            }
            let f = enc
                .encode(&pattern_frame(w, h, i), force_key)
                .unwrap()
                .expect("expected 1-in-1-out frame");
            assert!(!f.data.is_empty(), "{tag} frame {i} empty");
            if force_key {
                assert!(f.key, "{tag} frame {i} must be keyframe");
                assert!(
                    keyframe_has_headers(codec, &f.data),
                    "{tag} frame {i} missing parameter sets / sequence header"
                );
            }
            stream.extend_from_slice(&f.data);
        }
        assert_eq!(enc.active_kbps, 2000, "{tag}: bitrate should apply on keyframe");
        if let Some(n) = ffmpeg_strict_decode_count(codec, &stream, tag) {
            assert_eq!(n, 10, "{tag}: decoded frame count mismatch");
        }
        verify_red_blue_roundtrip(&mut enc, tag);
    }

    /// 验证 VA-API 编码器（H.264 / HEVC / AV1），包含非 16 对齐步长分辨率与严格解码/色彩校验
    #[test]
    fn vaapi_codecs_and_strides() {
        let Some(api) = ffmpeg::api() else {
            println!("FFmpeg unavailable, skipping VA-API test");
            return;
        };
        let nodes = discover_vaapi_nodes(None);
        if nodes.is_empty() {
            println!("No VA-API render nodes, skipping");
            return;
        }
        let mut tested = 0;
        for codec in [Codec::H264, Codec::H265, Codec::AV1] {
            match LinuxHwEncoder::new_vaapi(codec, 1280, 720, 30, 6000, None) {
                Ok(enc) => {
                    println!("VA-API {codec:?}: {}", enc.info.name);
                    exercise_encoder(enc, &format!("vaapi-{codec:?}-720p"));
                    tested += 1;
                }
                Err(e) => println!("VA-API {codec:?} unavailable: {e:#}"),
            }
        }
        // 非 16 对齐分辨率（覆盖 UV scratch 路径，见 P1-13）与 1440p（含 AV1）
        if tested > 0 {
            for (codec, w, h) in [
                (Codec::H264, 1366u32, 768u32),
                (Codec::H264, 1590, 994),
                (Codec::H264, 1728, 1080),
                (Codec::AV1, 1590, 994),
                (Codec::AV1, 2560, 1440),
            ] {
                if let Ok(enc) = LinuxHwEncoder::new_vaapi(codec, w, h, 30, 8000, None) {
                    exercise_encoder(enc, &format!("vaapi-{codec:?}-{w}x{h}"));
                }
            }
        }
        let _ = api;
    }

    /// 验证 NVENC 编码器（H.264 / HEVC / AV1），包含非 16 对齐步长分辨率与严格解码/色彩校验
    #[test]
    fn nvenc_codecs_and_strides() {
        let Some(_api) = ffmpeg::api() else {
            println!("FFmpeg unavailable, skipping NVENC test");
            return;
        };
        let mut tested = 0;
        for codec in [Codec::H264, Codec::H265, Codec::AV1] {
            match LinuxHwEncoder::new_nvenc(codec, 1280, 720, 30, 6000) {
                Ok(enc) => {
                    println!("NVENC {codec:?}: {}", enc.info.name);
                    exercise_encoder(enc, &format!("nvenc-{codec:?}-720p"));
                    tested += 1;
                }
                Err(e) => println!("NVENC {codec:?} unavailable: {e:#}"),
            }
        }
        if tested > 0 {
            for (codec, w, h) in [
                (Codec::H264, 1366u32, 768u32),
                (Codec::H264, 1590, 994),
                (Codec::H264, 1728, 1080),
                (Codec::AV1, 1590, 994),
                (Codec::AV1, 2560, 1440),
            ] {
                if let Ok(enc) = LinuxHwEncoder::new_nvenc(codec, w, h, 30, 8000) {
                    exercise_encoder(enc, &format!("nvenc-{codec:?}-{w}x{h}"));
                }
            }
        }
    }

    /// 验证瞬时码率抖动不触发多余关键帧，而持续降码率（≥ 1.25× / 2s）和持续升码率（≥ 1.3× / 6s）均会生效
    #[test]
    fn bitrate_hysteresis_avoids_congestion_idr_storm() {
        let Ok(mut enc) = LinuxHwEncoder::new(Codec::H264, 640, 360, 30, 8000) else {
            println!("No Linux hardware encoder available, skipping hysteresis test");
            return;
        };
        let f0 = enc.encode(&pattern_frame(640, 360, 0), true).unwrap().unwrap();
        assert!(f0.key);

        // 小幅波动（< 1.25× 降 / < 1.3× 升）不计时、不发关键帧
        enc.set_bitrate(7_000);
        assert!(enc.pending_bitrate.is_none());
        let f1 = enc.encode(&pattern_frame(640, 360, 1), false).unwrap().unwrap();
        assert!(!f1.key, "small dip must not emit an unsolicited keyframe");
        assert_eq!(enc.active_kbps, 8000);

        // 中等降码率（8000 -> 5000，1.6×）刚发生时还未到 2 秒：不立刻发 IDR
        enc.set_bitrate(5_000);
        assert!(matches!(enc.pending_bitrate, Some((_, BitrateDir::Drop))));
        let f2 = enc.encode(&pattern_frame(640, 360, 2), false).unwrap().unwrap();
        assert!(!f2.key, "transient drop must wait for hold window");
        assert_eq!(enc.active_kbps, 8000);

        // 模拟降码率持续超过 2 秒：生效并重置计时
        enc.pending_bitrate = Some((
            Instant::now() - BITRATE_DROP_HOLD - Duration::from_millis(50),
            BitrateDir::Drop,
        ));
        let f3 = enc.encode(&pattern_frame(640, 360, 3), false).unwrap().unwrap();
        assert!(f3.key, "sustained >=1.25x drop should reconfigure cleanly");
        assert_eq!(enc.active_kbps, 5_000);
        assert!(enc.pending_bitrate.is_none());

        // 升码率（5000 -> 8000，1.6×）刚发生时还未到 6 秒：不立刻发 IDR
        enc.set_bitrate(8_000);
        assert!(matches!(enc.pending_bitrate, Some((_, BitrateDir::Raise))));
        let f4 = enc.encode(&pattern_frame(640, 360, 4), false).unwrap().unwrap();
        assert!(!f4.key, "transient raise must wait for raise hold window");
        assert_eq!(enc.active_kbps, 5_000);

        // 模拟升码率持续超过 6 秒：恢复到高码率
        enc.pending_bitrate = Some((
            Instant::now() - BITRATE_RAISE_HOLD - Duration::from_millis(50),
            BitrateDir::Raise,
        ));
        let f5 = enc.encode(&pattern_frame(640, 360, 5), false).unwrap().unwrap();
        assert!(f5.key, "sustained >=1.3x raise should apply after hold window");
        assert_eq!(enc.active_kbps, 8_000);
        assert!(enc.pending_bitrate.is_none());
    }
}
