//! 扩展虚拟显示器（Linux X11）：通过 RandR 在空闲的 CRTC 和虚拟输出上添加自定义分辨率模式，
//! 把平板 / 另一台设备变成真实的 X11 扩展副屏（鼠标可自由移入、窗口可拖入，本机主屏保持不变）。
//!
//! # 输出选择
//!
//! 只使用明确的虚拟输出（绝不启用断开的物理 DP / HDMI 接口，也不强行点亮用户手动关闭的物理面板如 `eDP-1` / 外接电视）：
//! 1. 名字以 `VIRTUAL` / `Virtual` / `DUMMY` / `HEADLESS` 开头的虚拟输出（例如 `xf86-video-intel` 的 `VirtualHeads`、
//!    `xserver-xorg-video-dummy` 的 `DUMMY1`、`vkms` 的 `Virtual-1`）；
//! 2. 属于已知虚拟 RandR Provider（`evdi` / `vkms`）的空闲输出。
//!
//! [`usable`] 严格只读，绝不改动 GPU Provider 路由。仅在 [`activate`] 找不到已绑定虚拟输出时，
//! 才按需把虚拟 `SINK_OUTPUT` Provider（`evdi` / `vkms`）挂到主 `SOURCE_OUTPUT` 上，并在关闭时解绑。
//! 切换分辨率或旋转时原地重配 CRTC（不先关屏），避免窗口管理器把副屏上的窗口赶回主屏。
//! 所有创建的 RandR 模式名以 [`MODE_PREFIX`] 开头：关闭或下次启动 [`recover`] 时清理，不留残留。

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use airhand_proto::session::VirtualDisplaySpec;
use anyhow::{Context, Result, anyhow, bail};
use tracing::{info, warn};
use x11rb::protocol::randr::{
    self, Connection, ConnectionExt as _, ModeFlag, ModeInfo, ProviderCapability, Rotation,
};

use super::HUB;
use crate::host::x11::{self, X11};

/// 本程序创建的 RandR 模式名前缀（关闭及崩溃恢复时据此识别并清理）
const MODE_PREFIX: &str = "AIRHAND_";
/// 没事可做时等待超时（有请求、退出时会立刻唤醒）
const IDLE: Duration = Duration::from_secs(30);
/// 已开启时定时检查虚拟输出是否还在线（被别处关掉或拔掉时及时反映状态）
const WATCH: Duration = Duration::from_secs(2);

/// 本进程当前是否正占用着一块扩展虚拟输出（[`usable`] 据此保证不会被自己的占用误判为不可用）
static ACTIVE_ON: AtomicBool = AtomicBool::new(false);

/// 启用副屏前的根屏幕尺寸与物理毫米尺寸（缩放/关闭时保持原物理 DPI，且不把用户原本更大的帧缓冲缩坏）
#[derive(Debug, Clone, Copy)]
struct SavedScreen {
    width: u16,
    height: u16,
    mm_width: u32,
    mm_height: u32,
}

/// 正在运行的 X11 扩展虚拟显示器
struct Active {
    spec: VirtualDisplaySpec,
    output: randr::Output,
    output_name: String,
    crtc: randr::Crtc,
    mode: randr::Mode,
    saved: SavedScreen,
    /// 本次启用时主动绑定的虚拟 Sink Provider（关闭时解绑）
    attached_providers: Vec<randr::Provider>,
}

/// 候选的空闲虚拟输出与 CRTC
#[derive(Debug, Clone)]
struct Candidate {
    output: randr::Output,
    name: String,
    crtc: randr::Crtc,
    connected: bool,
}

/// 是否存在可用的 X11 RandR 虚拟输出和 CRTC（严格只读，不修改任何 Provider 或模式）
pub(super) fn usable() -> bool {
    if ACTIVE_ON.load(Ordering::Relaxed) {
        return true;
    }
    let Ok(x) = X11::connect() else {
        return false;
    };
    if check_randr(&x).is_err() {
        return false;
    }
    find_candidates(&x).is_ok_and(|c| !c.is_empty()) || has_attachable_virtual_provider(&x)
}

pub(super) fn spawn() {
    if let Err(e) = std::thread::Builder::new()
        .name("airhand-virtual-display".into())
        .spawn(run)
    {
        warn!("can't start the Linux virtual display manager: {e}");
        let mut w = HUB.wants.lock().unwrap();
        w.running = false;
    }
}

fn bump() {
    HUB.displays.send_modify(|g| *g = g.wrapping_add(1));
}

/// 启动时清理上次异常退出遗留的 `AIRHAND_*` 模式（如果还挂在某个 CRTC 上则先关闭它）
pub(super) fn recover() {
    let Ok(x) = X11::connect() else {
        return;
    };
    if let Err(e) = cleanup_leftovers(&x) {
        warn!("can't clean up leftover X11 virtual display modes: {e:#}");
    }
}

fn run() {
    info!("Linux virtual display manager started");
    let mut active: Option<Active> = None;
    loop {
        let now = Instant::now();
        let (wanted, stopping) = {
            let w = HUB.wants.lock().unwrap();
            let stopping = w.stopping;
            let wanted = if stopping { None } else { w.extend_wanted(now) };
            (wanted, stopping)
        };

        // 如果之前开启了，但外部（如 GNOME/KDE 显示管理或 xrandr）把它关掉了：
        // 清理残留并汇报 Failed，而不是每 2 秒重新强行开启与桌面环境打架
        if let Some(a) = &active
            && !is_active_still_on(a)
        {
            warn!(output = %a.output_name, "the X11 virtual display was turned off externally");
            if let Some(old) = active.take() {
                ACTIVE_ON.store(false, Ordering::Relaxed);
                deactivate(old);
            }
            super::fail_extend("the extended virtual display was turned off externally".into());
            bump();
            if stopping {
                break;
            }
            wait(IDLE);
            continue;
        }

        match (&mut active, wanted) {
            (Some(a), Some(spec)) if a.spec == spec => {
                // 已就绪，确保状态已发布
                let mut w = HUB.wants.lock().unwrap();
                let target = Some((spec, a.output_name.clone()));
                if w.extend_active != target {
                    w.extend_active = target;
                    super::publish(&w);
                }
            }
            (Some(a), Some(spec)) => {
                // 分辨率或横竖屏切换：优先原地重配同一 CRTC（不断开输出，副屏窗口不会被赶回主屏）
                match reconfigure_in_place(a, spec) {
                    Ok(()) => {
                        let name = a.output_name.clone();
                        info!(
                            output = %name,
                            w = spec.width,
                            h = spec.height,
                            hz = spec.hz,
                            "X11 extended virtual display reconfigured in place"
                        );
                        let mut w = HUB.wants.lock().unwrap();
                        w.extend_active = Some((spec, name));
                        w.extend_failed = None;
                        super::publish(&w);
                        bump();
                    }
                    Err(e) => {
                        warn!(
                            output = %a.output_name,
                            "in-place X11 resize failed ({e:#}), recreating virtual display"
                        );
                        if let Some(old) = active.take() {
                            ACTIVE_ON.store(false, Ordering::Relaxed);
                            deactivate(old);
                        }
                        match activate(spec) {
                            Ok(created) => {
                                let name = created.output_name.clone();
                                info!(
                                    output = %name,
                                    w = spec.width,
                                    h = spec.height,
                                    hz = spec.hz,
                                    "X11 extended virtual display active"
                                );
                                ACTIVE_ON.store(true, Ordering::Relaxed);
                                active = Some(created);
                                let mut w = HUB.wants.lock().unwrap();
                                w.extend_active = Some((spec, name));
                                w.extend_failed = None;
                                super::publish(&w);
                                bump();
                            }
                            Err(e) => {
                                super::fail_extend(format!("{e:#}"));
                            }
                        }
                    }
                }
            }
            (None, Some(spec)) => match activate(spec) {
                Ok(created) => {
                    let name = created.output_name.clone();
                    info!(
                        output = %name,
                        w = spec.width,
                        h = spec.height,
                        hz = spec.hz,
                        "X11 extended virtual display active"
                    );
                    ACTIVE_ON.store(true, Ordering::Relaxed);
                    active = Some(created);
                    let mut w = HUB.wants.lock().unwrap();
                    w.extend_active = Some((spec, name));
                    w.extend_failed = None;
                    super::publish(&w);
                    bump();
                }
                Err(e) => {
                    super::fail_extend(format!("{e:#}"));
                }
            },
            (Some(_), None) => {
                if let Some(old) = active.take() {
                    info!(output = %old.output_name, "disabling X11 extended virtual display");
                    ACTIVE_ON.store(false, Ordering::Relaxed);
                    deactivate(old);
                }
                let mut w = HUB.wants.lock().unwrap();
                w.extend_active = None;
                super::publish(&w);
                bump();
            }
            (None, None) => {}
        }

        if stopping && active.is_none() {
            break;
        }
        // 没事可做且没有活跃会话时退出线程（下次请求时再启动）
        if active.is_none() {
            let mut w = HUB.wants.lock().unwrap();
            if w.stopping || (w.sessions.is_empty() && w.extend_wanted(Instant::now()).is_none()) {
                w.running = false;
                w.extend_active = None;
                super::publish(&w);
                HUB.wake.notify_all();
                info!("Linux virtual display manager stopped");
                return;
            }
        }

        wait(if active.is_some() { WATCH } else { IDLE });
    }

    if let Some(old) = active.take() {
        ACTIVE_ON.store(false, Ordering::Relaxed);
        deactivate(old);
    }
    let mut w = HUB.wants.lock().unwrap();
    w.running = false;
    w.extend_active = None;
    super::publish(&w);
    HUB.wake.notify_all();
    info!("Linux virtual display manager stopped");
}

fn wait(period: Duration) {
    let mut w = HUB.wants.lock().unwrap();
    if w.stopping {
        drop(w);
        std::thread::sleep(Duration::from_millis(100));
        return;
    }
    if !w.poked {
        w = HUB
            .wake
            .wait_timeout_while(w, period, |w| !w.poked && !w.stopping)
            .unwrap()
            .0;
    }
    w.poked = false;
}

fn check_randr(x: &X11) -> Result<()> {
    if !x.has_extension(randr::X11_EXTENSION_NAME) {
        bail!("the X11 server has no RandR extension");
    }
    let v = x.conn.randr_query_version(1, 5)?.reply()?;
    if (v.major_version, v.minor_version) < (1, 4) {
        bail!(
            "RandR {}.{} is too old (need at least 1.4)",
            v.major_version,
            v.minor_version
        );
    }
    Ok(())
}

/// 识别已知的虚拟显示输出 Provider（如 `evdi` / `vkms`），不碰双显卡 PRIME/NVIDIA 等物理 GPU
fn is_virtual_provider_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("evdi") || lower.contains("vkms")
}

fn is_virtual_output_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("virtual") || lower.starts_with("dummy") || lower.starts_with("headless")
}

/// 只读：列出属于已知虚拟 Provider（`evdi` / `vkms`）的所有输出
fn virtual_provider_outputs(x: &X11) -> HashSet<randr::Output> {
    let mut set = HashSet::new();
    let Some(provs) = x
        .conn
        .randr_get_providers(x.root())
        .ok()
        .and_then(|c| c.reply().ok())
    else {
        return set;
    };
    for &p in &provs.providers {
        let Some(info) = x
            .conn
            .randr_get_provider_info(p, provs.timestamp)
            .ok()
            .and_then(|c| c.reply().ok())
        else {
            continue;
        };
        let name = String::from_utf8_lossy(&info.name);
        if is_virtual_provider_name(&name) {
            set.extend(info.outputs);
        }
    }
    set
}

/// 只读：是否存在未绑定但可按需挂到主 `SOURCE_OUTPUT` 的虚拟 `SINK_OUTPUT` Provider（`evdi` / `vkms`）
fn has_attachable_virtual_provider(x: &X11) -> bool {
    let Some(provs) = x
        .conn
        .randr_get_providers(x.root())
        .ok()
        .and_then(|c| c.reply().ok())
    else {
        return false;
    };
    if provs.providers.len() < 2 {
        return false;
    }
    let mut has_source = false;
    let mut has_virtual_sink = false;
    for &p in &provs.providers {
        let Some(info) = x
            .conn
            .randr_get_provider_info(p, provs.timestamp)
            .ok()
            .and_then(|c| c.reply().ok())
        else {
            continue;
        };
        let caps = info.capabilities;
        let name = String::from_utf8_lossy(&info.name);
        if caps.contains(ProviderCapability::SOURCE_OUTPUT) && !is_virtual_provider_name(&name) {
            has_source = true;
        }
        if is_virtual_provider_name(&name)
            && caps.contains(ProviderCapability::SINK_OUTPUT)
            && info.associated_providers.is_empty()
        {
            has_virtual_sink = true;
        }
    }
    has_source && has_virtual_sink
}

/// 按需将未绑定的虚拟 `SINK_OUTPUT` Provider（仅 `evdi` / `vkms`）挂到主 Source Output Provider，返回本次绑定的列表
fn attach_virtual_sink_providers(x: &X11) -> Vec<randr::Provider> {
    let root = x.root();
    let Some(provs) = x
        .conn
        .randr_get_providers(root)
        .ok()
        .and_then(|c| c.reply().ok())
    else {
        return Vec::new();
    };
    if provs.providers.len() < 2 {
        return Vec::new();
    }
    let mut source = None;
    let mut sinks = Vec::new();
    for &p in &provs.providers {
        let Some(info) = x
            .conn
            .randr_get_provider_info(p, provs.timestamp)
            .ok()
            .and_then(|c| c.reply().ok())
        else {
            continue;
        };
        let caps = info.capabilities;
        let name = String::from_utf8_lossy(&info.name);
        if source.is_none()
            && caps.contains(ProviderCapability::SOURCE_OUTPUT)
            && !is_virtual_provider_name(&name)
        {
            source = Some(p);
        } else if is_virtual_provider_name(&name)
            && caps.contains(ProviderCapability::SINK_OUTPUT)
            && info.associated_providers.is_empty()
        {
            sinks.push(p);
        }
    }
    let mut attached = Vec::new();
    if let Some(src) = source {
        for sink in sinks {
            if x.conn
                .randr_set_provider_output_source(sink, src, provs.timestamp)
                .ok()
                .and_then(|c| c.check().ok())
                .is_some()
            {
                attached.push(sink);
            }
        }
        if !attached.is_empty() {
            let _ = x.sync();
        }
    }
    attached
}

fn detach_providers(x: &X11, providers: &[randr::Provider]) {
    if providers.is_empty() {
        return;
    }
    let Some(provs) = x
        .conn
        .randr_get_providers(x.root())
        .ok()
        .and_then(|c| c.reply().ok())
    else {
        return;
    };
    for &sink in providers {
        let _ = x
            .conn
            .randr_set_provider_output_source(sink, x11rb::NONE, provs.timestamp);
    }
    let _ = x.sync();
}

/// 只读：列出所有空闲的虚拟 RandR 输出与 CRTC（不触碰任何物理输出）
fn find_candidates(x: &X11) -> Result<Vec<Candidate>> {
    check_randr(x)?;
    let root = x.root();
    let res = x.conn.randr_get_screen_resources_current(root)?.reply()?;

    let mut free_crtcs = Vec::new();
    for &crtc in &res.crtcs {
        if let Ok(info) = x.conn.randr_get_crtc_info(crtc, res.config_timestamp)?.reply()
            && info.mode == 0
            && info.outputs.is_empty()
        {
            free_crtcs.push(crtc);
        }
    }
    if free_crtcs.is_empty() {
        return Ok(Vec::new());
    }

    let vprov_outputs = virtual_provider_outputs(x);
    let mut out = Vec::new();
    for &output in &res.outputs {
        let Ok(info) = x
            .conn
            .randr_get_output_info(output, res.config_timestamp)?
            .reply()
        else {
            continue;
        };
        if info.crtc != 0 {
            continue;
        }
        let Some(&crtc) = info.crtcs.iter().find(|c| free_crtcs.contains(c)) else {
            continue;
        };
        let name = String::from_utf8_lossy(&info.name).into_owned();
        if !is_virtual_output_name(&name) && !vprov_outputs.contains(&output) {
            continue;
        }
        let connected = info.connection == Connection::CONNECTED;
        out.push(Candidate {
            output,
            name,
            crtc,
            connected,
        });
    }

    // 优先级：已连接的虚拟输出（如 vkms / evdi）> 断开的虚拟头（如 VIRTUAL1 / DUMMY1）
    out.sort_by_key(|c| (!c.connected, c.name.clone()));
    Ok(out)
}

fn activate(spec: VirtualDisplaySpec) -> Result<Active> {
    let spec = spec.sanitized();
    let x = X11::connect()?;
    let mut attached_providers = Vec::new();
    let mut candidates = find_candidates(&x)?;
    if candidates.is_empty() {
        attached_providers = attach_virtual_sink_providers(&x);
        if !attached_providers.is_empty() {
            candidates = find_candidates(&x)?;
        }
    }
    if candidates.is_empty() {
        detach_providers(&x, &attached_providers);
        bail!(
            "no free virtual X11 RandR output was found (configure VirtualHeads, evdi, vkms, or xserver-xorg-video-dummy)"
        );
    }

    let mut last_err = None;
    for cand in candidates {
        match try_activate_candidate(&x, &cand, spec, attached_providers.clone()) {
            Ok(a) => return Ok(a),
            Err(e) => {
                warn!(output = %cand.name, "failed to activate X11 output: {e:#}");
                last_err = Some(e);
            }
        }
    }
    detach_providers(&x, &attached_providers);
    Err(last_err.unwrap_or_else(|| anyhow!("no usable X11 output accepted the virtual display")))
}

/// 校验扩展桌面尺寸（用 `i32` 防 `u16` 溢出），并保留至少原本的屏幕尺寸（不把用户原本更大的帧缓冲缩坏）
fn compute_target_screen(
    place_x: i32,
    other_max_y: i32,
    spec: VirtualDisplaySpec,
    saved: SavedScreen,
    range: &randr::GetScreenSizeRangeReply,
) -> Result<(u16, u16)> {
    let req_w = place_x
        .checked_add(spec.width as i32)
        .ok_or_else(|| anyhow!("extended desktop width overflow"))?;
    let req_h = other_max_y.max(spec.height as i32);
    if req_w <= 0
        || req_h <= 0
        || req_w > range.max_width as i32
        || req_h > range.max_height as i32
    {
        bail!(
            "extended desktop {}x{} exceeds X11 maximum {}x{}",
            req_w,
            req_h,
            range.max_width,
            range.max_height
        );
    }
    let w = (req_w as u16)
        .max(saved.width)
        .clamp(range.min_width, range.max_width);
    let h = (req_h as u16)
        .max(saved.height)
        .clamp(range.min_height, range.max_height);
    Ok((w, h))
}

fn try_activate_candidate(
    x: &X11,
    cand: &Candidate,
    spec: VirtualDisplaySpec,
    attached_providers: Vec<randr::Provider>,
) -> Result<Active> {
    let root = x.root();
    let res = x.conn.randr_get_screen_resources(root)?.reply()?;
    let saved = SavedScreen {
        width: x.screen().width_in_pixels,
        height: x.screen().height_in_pixels,
        mm_width: x.screen().width_in_millimeters as u32,
        mm_height: x.screen().height_in_millimeters as u32,
    };

    // 计算当前活动 CRTC 的外接矩形，将副屏放在最右侧（不动现有物理显示器的位置与模式）
    let active_bounds = active_crtc_bounds(x, &res, Some(cand.crtc));
    let place_x = active_bounds.max_x;
    let place_y = 0i32;

    let range = x.conn.randr_get_screen_size_range(root)?.reply()?;
    let (target_w, target_h) =
        compute_target_screen(place_x, active_bounds.max_y, spec, saved, &range)?;

    let mode_name = format!("{MODE_PREFIX}{}x{}_{}", spec.width, spec.height, spec.hz);
    let mode_id = ensure_mode(x, &res, &mode_name, spec)?;

    if let Err(e) = x
        .conn
        .randr_add_output_mode(cand.output, mode_id)?
        .check()
    {
        let _ = x.conn.randr_destroy_mode(mode_id);
        return Err(e).with_context(|| format!("RandR AddOutputMode on {}", cand.name));
    }

    if target_w > saved.width || target_h > saved.height {
        let fb_w = target_w.max(saved.width);
        let fb_h = target_h.max(saved.height);
        x.conn
            .randr_set_screen_size(
                root,
                fb_w,
                fb_h,
                scaled_mm(fb_w, saved.width, saved.mm_width),
                scaled_mm(fb_h, saved.height, saved.mm_height),
            )?
            .check()
            .context("RandR SetScreenSize")?;
    }

    // 刷新资源获取最新时间戳，启用 CRTC
    let res2 = x.conn.randr_get_screen_resources_current(root)?.reply()?;
    let reply = x
        .conn
        .randr_set_crtc_config(
            cand.crtc,
            x11rb::CURRENT_TIME,
            res2.config_timestamp,
            place_x as i16,
            place_y as i16,
            mode_id,
            Rotation::ROTATE0,
            &[cand.output],
        )?
        .reply();

    match reply {
        Ok(r) if r.status == randr::SetConfig::SUCCESS => {
            if target_w != saved.width || target_h != saved.height {
                let _ = x.conn.randr_set_screen_size(
                    root,
                    target_w,
                    target_h,
                    scaled_mm(target_w, saved.width, saved.mm_width),
                    scaled_mm(target_h, saved.height, saved.mm_height),
                );
            }
            x.sync()?;
            Ok(Active {
                spec,
                output: cand.output,
                output_name: cand.name.clone(),
                crtc: cand.crtc,
                mode: mode_id,
                saved,
                attached_providers,
            })
        }
        other => {
            let _ = x.conn.randr_delete_output_mode(cand.output, mode_id);
            let _ = x.conn.randr_destroy_mode(mode_id);
            restore_screen_size(x, Some(saved));
            let _ = x.sync();
            bail!(
                "cannot activate X11 output {} ({:?})",
                cand.name,
                other.map(|r| r.status)
            );
        }
    }
}

/// 原地切换已开启副屏的分辨率 / 横竖屏：保持 CRTC 始终开启，避免窗口管理器将副屏窗口移回主屏
fn reconfigure_in_place(a: &mut Active, spec: VirtualDisplaySpec) -> Result<()> {
    let spec = spec.sanitized();
    let x = X11::connect()?;
    check_randr(&x)?;
    let root = x.root();
    let res = x.conn.randr_get_screen_resources(root)?.reply()?;
    let range = x.conn.randr_get_screen_size_range(root)?.reply()?;

    let other_bounds = active_crtc_bounds(&x, &res, Some(a.crtc));
    let place_x = other_bounds.max_x;
    let place_y = 0i32;
    let (target_w, target_h) =
        compute_target_screen(place_x, other_bounds.max_y, spec, a.saved, &range)?;

    let mode_name = format!("{MODE_PREFIX}{}x{}_{}", spec.width, spec.height, spec.hz);
    let new_mode = ensure_mode(&x, &res, &mode_name, spec)?;
    if new_mode != a.mode
        && let Err(e) = x
            .conn
            .randr_add_output_mode(a.output, new_mode)?
            .check()
    {
        let _ = x.conn.randr_destroy_mode(new_mode);
        return Err(e).with_context(|| format!("RandR AddOutputMode on {}", a.output_name));
    }

    let cur_w = x.screen().width_in_pixels;
    let cur_h = x.screen().height_in_pixels;
    let grow_w = cur_w.max(target_w);
    let grow_h = cur_h.max(target_h);
    if grow_w > cur_w || grow_h > cur_h {
        if let Err(e) = x
            .conn
            .randr_set_screen_size(
                root,
                grow_w,
                grow_h,
                scaled_mm(grow_w, a.saved.width, a.saved.mm_width),
                scaled_mm(grow_h, a.saved.height, a.saved.mm_height),
            )?
            .check()
        {
            if new_mode != a.mode {
                let _ = x.conn.randr_delete_output_mode(a.output, new_mode);
                let _ = x.conn.randr_destroy_mode(new_mode);
            }
            return Err(e).context("RandR SetScreenSize (grow)");
        }
    }

    let res2 = x.conn.randr_get_screen_resources_current(root)?.reply()?;
    let reply = x
        .conn
        .randr_set_crtc_config(
            a.crtc,
            x11rb::CURRENT_TIME,
            res2.config_timestamp,
            place_x as i16,
            place_y as i16,
            new_mode,
            Rotation::ROTATE0,
            &[a.output],
        )?
        .reply();

    match reply {
        Ok(r) if r.status == randr::SetConfig::SUCCESS => {
            if target_w != grow_w || target_h != grow_h {
                let _ = x.conn.randr_set_screen_size(
                    root,
                    target_w,
                    target_h,
                    scaled_mm(target_w, a.saved.width, a.saved.mm_width),
                    scaled_mm(target_h, a.saved.height, a.saved.mm_height),
                );
            }
            let old_mode = a.mode;
            a.spec = spec;
            a.mode = new_mode;
            if old_mode != new_mode {
                let _ = x.conn.randr_delete_output_mode(a.output, old_mode);
                let _ = x.conn.randr_destroy_mode(old_mode);
            }
            x.sync()?;
            Ok(())
        }
        other => {
            if new_mode != a.mode {
                let _ = x.conn.randr_delete_output_mode(a.output, new_mode);
                let _ = x.conn.randr_destroy_mode(new_mode);
            }
            bail!(
                "in-place SetCrtcConfig failed on {} ({:?})",
                a.output_name,
                other.map(|r| r.status)
            );
        }
    }
}

fn deactivate(a: Active) {
    if let Ok(x) = X11::connect() {
        let root = x.root();
        if let Some(res) = x
            .conn
            .randr_get_screen_resources_current(root)
            .ok()
            .and_then(|c| c.reply().ok())
        {
            let _ = x.conn.randr_set_crtc_config(
                a.crtc,
                x11rb::CURRENT_TIME,
                res.config_timestamp,
                0,
                0,
                x11rb::NONE,
                Rotation::ROTATE0,
                &[],
            );
            let _ = x.conn.randr_delete_output_mode(a.output, a.mode);
            let _ = x.conn.randr_destroy_mode(a.mode);
            restore_screen_size(&x, Some(a.saved));
            let _ = x.sync();
        }
        detach_providers(&x, &a.attached_providers);
    }
}

fn is_active_still_on(a: &Active) -> bool {
    let Ok(x) = X11::connect() else {
        return false;
    };
    let Ok(mons) = x11::monitors(&x) else {
        return false;
    };
    mons.iter().any(|m| m.name == a.output_name)
}

fn cleanup_leftovers(x: &X11) -> Result<()> {
    check_randr(x)?;
    let root = x.root();
    let res = x.conn.randr_get_screen_resources_current(root)?.reply()?;
    let leftover_modes = find_prefixed_modes(&res, MODE_PREFIX);
    if leftover_modes.is_empty() {
        return Ok(());
    }
    let mut disabled_any = false;
    for &crtc in &res.crtcs {
        if let Ok(info) = x.conn.randr_get_crtc_info(crtc, res.config_timestamp)?.reply()
            && leftover_modes.contains(&info.mode)
        {
            info!(
                crtc,
                mode = info.mode,
                "cleaning up leftover Airhand virtual display CRTC"
            );
            let _ = x.conn.randr_set_crtc_config(
                crtc,
                x11rb::CURRENT_TIME,
                res.config_timestamp,
                0,
                0,
                x11rb::NONE,
                Rotation::ROTATE0,
                &[],
            );
            disabled_any = true;
        }
    }
    for &output in &res.outputs {
        for &mode in &leftover_modes {
            let _ = x.conn.randr_delete_output_mode(output, mode);
        }
    }
    for &mode in &leftover_modes {
        let _ = x.conn.randr_destroy_mode(mode);
    }
    if disabled_any {
        restore_screen_size(x, None);
    }
    let _ = x.sync();
    Ok(())
}

fn find_prefixed_modes(
    res: &randr::GetScreenResourcesCurrentReply,
    prefix: &str,
) -> Vec<randr::Mode> {
    let mut offset = 0usize;
    let mut out = Vec::new();
    for m in &res.modes {
        let len = m.name_len as usize;
        if let Some(bytes) = res.names.get(offset..offset + len)
            && std::str::from_utf8(bytes).is_ok_and(|s| s.starts_with(prefix))
        {
            out.push(m.id);
        }
        offset += len;
    }
    out
}

fn find_named_mode(res: &randr::GetScreenResourcesReply, name: &str) -> Option<randr::Mode> {
    let mut offset = 0usize;
    for m in &res.modes {
        let len = m.name_len as usize;
        if let Some(bytes) = res.names.get(offset..offset + len)
            && bytes == name.as_bytes()
        {
            return Some(m.id);
        }
        offset += len;
    }
    None
}

fn ensure_mode(
    x: &X11,
    res: &randr::GetScreenResourcesReply,
    name: &str,
    spec: VirtualDisplaySpec,
) -> Result<randr::Mode> {
    if let Some(existing) = find_named_mode(res, name) {
        return Ok(existing);
    }
    let info = cvt_rb_mode_info(spec.width, spec.height, spec.hz, name.len() as u16);
    let reply = x
        .conn
        .randr_create_mode(x.root(), info, name.as_bytes())?
        .reply()
        .context("RandR CreateMode")?;
    Ok(reply.mode)
}

#[derive(Default)]
struct Bounds {
    max_x: i32,
    max_y: i32,
}

fn active_crtc_bounds(
    x: &X11,
    res: &randr::GetScreenResourcesReply,
    skip_crtc: Option<randr::Crtc>,
) -> Bounds {
    let mut b = Bounds::default();
    for &crtc in &res.crtcs {
        if Some(crtc) == skip_crtc {
            continue;
        }
        if let Some(info) = x
            .conn
            .randr_get_crtc_info(crtc, res.config_timestamp)
            .ok()
            .and_then(|c| c.reply().ok())
            && info.mode != 0
            && info.width > 0
            && info.height > 0
        {
            b.max_x = b.max_x.max(info.x as i32 + info.width as i32);
            b.max_y = b.max_y.max(info.y as i32 + info.height as i32);
        }
    }
    b
}

/// 关闭副屏后恢复帧缓冲大小与物理毫米尺寸（若有保存的原始尺寸则精确恢复，并兼容期间接入的新显示器）
fn restore_screen_size(x: &X11, saved: Option<SavedScreen>) {
    let root = x.root();
    let Some(res) = x
        .conn
        .randr_get_screen_resources_current(root)
        .ok()
        .and_then(|c| c.reply().ok())
    else {
        return;
    };
    let Some(range) = x
        .conn
        .randr_get_screen_size_range(root)
        .ok()
        .and_then(|c| c.reply().ok())
    else {
        return;
    };
    let mut max_x = 0i32;
    let mut max_y = 0i32;
    for &crtc in &res.crtcs {
        if let Some(info) = x
            .conn
            .randr_get_crtc_info(crtc, res.config_timestamp)
            .ok()
            .and_then(|c| c.reply().ok())
            && info.mode != 0
            && info.width > 0
            && info.height > 0
        {
            max_x = max_x.max(info.x as i32 + info.width as i32);
            max_y = max_y.max(info.y as i32 + info.height as i32);
        }
    }
    let cur_w = x.screen().width_in_pixels;
    let cur_h = x.screen().height_in_pixels;
    let cur_mm_w = x.screen().width_in_millimeters as u32;
    let cur_mm_h = x.screen().height_in_millimeters as u32;
    if let Some(s) = saved {
        let w = (max_x.max(s.width as i32) as u16).clamp(range.min_width, range.max_width);
        let h = (max_y.max(s.height as i32) as u16).clamp(range.min_height, range.max_height);
        let mm_w = if w == s.width {
            s.mm_width.max(1)
        } else {
            scaled_mm(w, s.width, s.mm_width)
        };
        let mm_h = if h == s.height {
            s.mm_height.max(1)
        } else {
            scaled_mm(h, s.height, s.mm_height)
        };
        let _ = x.conn.randr_set_screen_size(root, w, h, mm_w, mm_h);
    } else if max_x > 0 && max_y > 0 {
        let w = (max_x as u16).clamp(range.min_width, range.max_width);
        let h = (max_y as u16).clamp(range.min_height, range.max_height);
        let _ = x.conn.randr_set_screen_size(
            root,
            w,
            h,
            scaled_mm(w, cur_w, cur_mm_w),
            scaled_mm(h, cur_h, cur_mm_h),
        );
    }
}

/// 按原屏幕的物理 DPI 等比换算毫米尺寸（读不出原尺寸时退回 96 DPI）
fn scaled_mm(px: u16, orig_px: u16, orig_mm: u32) -> u32 {
    if orig_px > 0 && orig_mm > 0 {
        ((px as f64) * (orig_mm as f64) / (orig_px as f64))
            .round()
            .max(1.0) as u32
    } else {
        ((px as f64) * 25.4 / 96.0).round().max(1.0) as u32
    }
}

/// 按 VESA CVT 1.2 Reduced Blanking 规范计算 RandR ModeInfo（与 `cvt -r` 一致）
pub(crate) fn cvt_rb_mode_info(width: u16, height: u16, hz: u8, name_len: u16) -> ModeInfo {
    const H_BLANK: u32 = 160;
    const H_FRONT_PORCH: u32 = 48;
    const H_SYNC: u32 = 32;
    const V_FRONT_PORCH: u32 = 3;
    const MIN_V_BLANK_US: f64 = 460.0;
    const MIN_V_BACK_PORCH: u32 = 6;
    const CLOCK_STEP_KHZ: f64 = 250.0;

    let w = width as u32;
    let h = height as u32;
    let v_field_rate = hz as f64;

    // 按宽高比选择标准垂直同步脉宽（CVT 1.2 表 3-3）
    let v_sync = if h * 4 == w * 3 {
        4
    } else if h * 16 == w * 9 {
        5
    } else if h * 16 == w * 10 {
        6
    } else if h * 5 == w * 4 || h * 15 == w * 9 {
        7
    } else {
        10
    };

    let h_period_est = ((1_000_000.0 / v_field_rate) - MIN_V_BLANK_US) / (h as f64);
    let mut vbi_lines = (MIN_V_BLANK_US / h_period_est).floor() as u32 + 1;
    let rb_min_vbi = V_FRONT_PORCH + v_sync + MIN_V_BACK_PORCH;
    if vbi_lines < rb_min_vbi {
        vbi_lines = rb_min_vbi;
    }
    let v_total = h + vbi_lines;
    let h_total = w + H_BLANK;

    let ideal_clock_khz = v_field_rate * (v_total as f64) * (h_total as f64) / 1000.0;
    let dot_clock_khz = (ideal_clock_khz / CLOCK_STEP_KHZ).floor() * CLOCK_STEP_KHZ;
    let dot_clock = (dot_clock_khz * 1000.0) as u32;

    ModeInfo {
        id: 0,
        width,
        height,
        dot_clock,
        hsync_start: (w + H_FRONT_PORCH) as u16,
        hsync_end: (w + H_FRONT_PORCH + H_SYNC) as u16,
        htotal: h_total as u16,
        hskew: 0,
        vsync_start: (h + V_FRONT_PORCH) as u16,
        vsync_end: (h + V_FRONT_PORCH + v_sync) as u16,
        vtotal: v_total as u16,
        name_len,
        mode_flags: ModeFlag::HSYNC_POSITIVE | ModeFlag::VSYNC_NEGATIVE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CVT-RB 时序与 `cvt -r 1920 1080 60` / `cvt -r 2560 1600 60` 的标准参数一致
    #[test]
    fn cvt_rb_timings_match_standard() {
        let m1080 = cvt_rb_mode_info(1920, 1080, 60, 16);
        assert_eq!((m1080.width, m1080.height), (1920, 1080));
        assert_eq!(m1080.hsync_start, 1968);
        assert_eq!(m1080.hsync_end, 2000);
        assert_eq!(m1080.htotal, 2080);
        assert_eq!(m1080.vsync_start, 1083);
        assert_eq!(m1080.vsync_end, 1088);
        assert_eq!(m1080.vtotal, 1111);
        assert_eq!(m1080.dot_clock, 138_500_000);

        // 竖屏也正常计算，不会溢出或产生非法时序
        let portrait = cvt_rb_mode_info(1200, 1920, 60, 16);
        assert_eq!((portrait.width, portrait.height), (1200, 1920));
        assert!(portrait.htotal > portrait.hsync_end);
        assert!(portrait.hsync_end > portrait.hsync_start);
        assert!(portrait.vtotal > portrait.vsync_end);
        assert!(portrait.vsync_end > portrait.vsync_start);
        assert!(portrait.dot_clock > 100_000_000);
    }

    #[test]
    fn virtual_output_and_provider_names_exclude_physical() {
        assert!(is_virtual_output_name("VIRTUAL1"));
        assert!(is_virtual_output_name("Virtual-1"));
        assert!(is_virtual_output_name("DUMMY1"));
        assert!(is_virtual_output_name("HEADLESS-1"));
        assert!(!is_virtual_output_name("HDMI-1"));
        assert!(!is_virtual_output_name("DP-1"));
        assert!(!is_virtual_output_name("eDP-1"));

        assert!(is_virtual_provider_name("evdi"));
        assert!(is_virtual_provider_name("VKMS"));
        assert!(!is_virtual_provider_name("modesetting"));
        assert!(!is_virtual_provider_name("NVIDIA-0"));
    }

    #[test]
    fn target_screen_checks_overflow_and_preserves_saved_fb() {
        let range = randr::GetScreenSizeRangeReply {
            sequence: 0,
            length: 0,
            min_width: 320,
            min_height: 200,
            max_width: 8192,
            max_height: 8192,
        };
        let saved = SavedScreen {
            width: 3840,
            height: 2160,
            mm_width: 600,
            mm_height: 340,
        };
        let spec = VirtualDisplaySpec {
            width: 2560,
            height: 1600,
            hz: 60,
            hidpi: false,
        };
        // 即使活动显示器 + 副屏小于 saved.height，也保留用户原有的帧缓冲尺寸
        let (w, h) = compute_target_screen(1920, 1080, spec, saved, &range).unwrap();
        assert_eq!((w, h), (4480, 2160));

        // 超过 u16::MAX 时在 i32 层直接报错而不截断溢出
        assert!(compute_target_screen(64000, 1080, spec, saved, &range).is_err());

        // 物理毫米按原 DPI 比例缩放
        assert_eq!(scaled_mm(3840, 1920, 500), 1000);
    }
}
