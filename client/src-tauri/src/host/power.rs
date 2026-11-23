//! 有控制端连着的时候不熄屏、不睡眠（Windows）。
//!
//! 熄了屏，桌面就不再刷新，桌面复制拿不到新的画面：控制端只看不动的时候过几分钟画面就停了，动一下鼠标才恢复。
//! 支持现代待机（S0 低功耗空闲）的电脑熄屏就等于待机，进程被暂停、网络断开，会话也就断了。所以有会话的时候
//! 向系统要两个电源请求（`PowerSetRequest`）：
//!
//! * `PowerRequestDisplayRequired`：不因为没人碰键盘鼠标而关闭显示器；
//! * `PowerRequestSystemRequired`：不因为空闲而睡眠（微软的文档：要保持屏幕亮着，两个都要）。
//!
//! 第一个会话开始时要，最后一个会话结束时放开（[`Awake`] 按会话计数）。会话开始时屏幕已经熄了的话还叫醒它一次
//! （[`wake_display`]），不然第一帧画面要等控制端动了鼠标才来；防窥模式下不叫（本机的显示器马上要撤下来，不能先亮一下）。
//!
//! 管不了的：合上盖子、按电源键、开始菜单里选"睡眠"这些用户自己让电脑睡眠的，所有电源请求都会被系统终止；
//! 用电池的现代待机电脑上，系统还会在睡眠超时 5 分钟后终止这些请求。被控端以 SYSTEM 身份在控制台会话里运行，
//! 锁屏不影响它的请求。

/// 有会话期间保持亮屏、不睡眠。所有的都丢掉了才放开
pub struct Awake(());

#[cfg(windows)]
mod imp {
    use std::sync::Mutex;

    use tracing::{info, warn};
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Power::{
        ES_DISPLAY_REQUIRED, PowerClearRequest, PowerCreateRequest, PowerRequestDisplayRequired,
        PowerRequestSystemRequired, PowerSetRequest, SetThreadExecutionState,
    };
    use windows::Win32::System::Threading::{
        POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
    };
    use windows::core::{PWSTR, w};

    /// 有几个 [`super::Awake`]，电源请求的句柄（`HANDLE` 不能跨线程，存它的值）
    static STATE: Mutex<(usize, usize)> = Mutex::new((0, 0));

    const REQUESTS: [windows::Win32::System::Power::POWER_REQUEST_TYPE; 2] =
        [PowerRequestSystemRequired, PowerRequestDisplayRequired];

    pub fn acquire() {
        let mut st = STATE.lock().unwrap();
        st.0 += 1;
        if st.0 > 1 {
            return;
        }
        let reason = REASON_CONTEXT {
            // POWER_REQUEST_CONTEXT_VERSION
            Version: 0,
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 {
                // `powercfg /requests` 里显示的原因。系统会复制一份
                SimpleReasonString: PWSTR(
                    w!("Airhand: a remote session is active").as_ptr() as *mut u16
                ),
            },
        };
        match unsafe { PowerCreateRequest(&reason) } {
            Ok(h) => {
                for r in REQUESTS {
                    if let Err(e) = unsafe { PowerSetRequest(h, r) } {
                        warn!("can't keep the computer awake ({r:?}): {e}");
                    }
                }
                st.1 = h.0 as usize;
                info!("keeping the display on while controllers are connected");
            }
            Err(e) => warn!("can't create a power request: {e}"),
        }
    }

    pub fn release() {
        let mut st = STATE.lock().unwrap();
        st.0 = st.0.saturating_sub(1);
        if st.0 > 0 || st.1 == 0 {
            return;
        }
        let h = HANDLE(std::mem::take(&mut st.1) as *mut _);
        unsafe {
            for r in REQUESTS {
                let _ = PowerClearRequest(h, r);
            }
            let _ = CloseHandle(h);
        }
        info!("the display may turn off again");
    }

    pub fn wake_display() {
        // 不带 ES_CONTINUOUS：只是当作有人碰了一下，熄了的屏幕亮起来、显示器的空闲计时从头开始
        unsafe { SetThreadExecutionState(ES_DISPLAY_REQUIRED) };
    }
}

/// 有会话期间保持亮屏、不睡眠（见模块说明）
pub fn keep_awake() -> Awake {
    #[cfg(windows)]
    imp::acquire();
    Awake(())
}

impl Drop for Awake {
    fn drop(&mut self) {
        #[cfg(windows)]
        imp::release();
    }
}

/// 屏幕熄了的话叫醒它（见模块说明）
pub fn wake_display() {
    #[cfg(windows)]
    imp::wake_display();
}
