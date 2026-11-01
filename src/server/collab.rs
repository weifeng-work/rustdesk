//! 协作模式（Collab Mode）—— 所有者 + 租客的控制权模型。
//!
//! 与「浏览模式（View Mode）」并列的第三种会话模式。默认关闭，由环境变量
//! `RUSTDESK_COLLAB=1` 开启。设计文档见仓库根目录 `COLLAB_MODE_DESIGN.md`。
//!
//! 三条不变量：
//!
//! 1. **单写者**：任何时刻最多一方能注入键鼠。
//! 2. **所有权优先**：所有者（被控端物理使用者）一动，租客立刻失去控制权。
//!    这不是缺陷——所有者输入来自硬件，软件层拦不住，详见设计文档 §2.4。
//! 3. **点击才抢占**：移动只更新虚拟光标位置，按下才把真实光标瞬移到租客位置、
//!    抬起后归还。键盘的所有权比鼠标多保留 [`GRANT_TIMEOUT`]，避免"点完就打字"
//!    的连续操作被打断。
//!
//! 所有状态集中在 [`STATE`] 一处，按连接隔离；禁止跨 `.await` 持锁。

use enigo::{Enigo, MouseControllable};
use lazy_static::lazy_static;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// 抬起后键盘所有权保留时长。
const GRANT_TIMEOUT: Duration = Duration::from_secs(5);
/// 抬起后归还真实光标前的缓冲，给目标程序留出处理 click 的时间。
const RESTORE_DELAY: Duration = Duration::from_millis(150);
/// 所有者光标位移超过该像素数即视为"所有者在动"。
const OWNER_ACTIVE_DISTANCE: i32 = 5;
/// 自身注入后多久内的输入变化不算所有者活动。
const INJECTION_SETTLE: Duration = Duration::from_millis(50);

struct Tenant {
    /// 租客最新意图坐标。
    cursor: (i32, i32),
}

struct Collab {
    inited: bool,
    enabled: bool,
    tenants: HashMap<i32, Tenant>,
    /// 键鼠注入权的持有者；`None` 表示无人持有。
    holder: Option<i32>,
    /// 持有者的过期时刻，到期自动释放。
    holder_deadline: Option<Instant>,
    /// 真实光标当前是否被租客占用（按下到抬起之间）。
    cursor_held: bool,
    /// 抢占前所有者光标所在位置，归还时用。
    owner_saved: Option<(i32, i32)>,
    /// 计划归还真实光标的时刻。
    restore_at: Option<Instant>,
    last_inject_at: Option<Instant>,
    last_owner_tick: Option<u32>,
    owner_pos: Option<(i32, i32)>,
}

impl Collab {
    fn new() -> Self {
        Self {
            inited: false,
            enabled: false,
            tenants: HashMap::new(),
            holder: None,
            holder_deadline: None,
            cursor_held: false,
            owner_saved: None,
            restore_at: None,
            last_inject_at: None,
            last_owner_tick: None,
            owner_pos: None,
        }
    }
}

lazy_static! {
    static ref STATE: Mutex<Collab> = Mutex::new(Collab::new());
}

fn state() -> MutexGuard<'static, Collab> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// 协作模式是否开启。首次调用时读环境变量，只读一次。
pub fn enabled() -> bool {
    let mut s = state();
    if !s.inited {
        s.inited = true;
        s.enabled = std::env::var("RUSTDESK_COLLAB")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        log::info!("collab mode enabled: {}", s.enabled);
    }
    s.enabled
}

/// 鼠标移动。返回 `true` 表示调用方照旧注入；`false` 表示只记录坐标。
pub fn on_move(conn: i32, x: i32, y: i32) -> bool {
    if !enabled() {
        return true;
    }
    let mut s = state();
    let entry = s.tenants.entry(conn).or_insert(Tenant { cursor: (x, y) });
    entry.cursor = (x, y);
    if s.cursor_held && s.holder == Some(conn) {
        // 拖拽期间必须持续注入，否则拖不动窗口或选区。
        s.last_inject_at = Some(Instant::now());
        true
    } else {
        false
    }
}

/// 鼠标按下。返回 `false` 表示拒绝本次注入。
pub fn admit_press(conn: i32) -> bool {
    if !enabled() {
        return true;
    }
    let mut s = state();
    if s.cursor_held && s.holder != Some(conn) {
        return false;
    }
    if s.owner_saved.is_none() {
        s.owner_saved = crate::get_cursor_pos();
    }
    s.holder = Some(conn);
    s.holder_deadline = None;
    s.cursor_held = true;
    s.restore_at = None;
    true
}

/// 按下前应当把真实光标移到的坐标；`None` 表示无需移动。
pub fn press_target(conn: i32) -> Option<(i32, i32)> {
    if !enabled() {
        return None;
    }
    let mut s = state();
    if !s.cursor_held || s.holder != Some(conn) {
        return None;
    }
    let target = s.tenants.get(&conn)?.cursor;
    s.last_inject_at = Some(Instant::now());
    Some(target)
}

/// 鼠标抬起。返回 `false` 表示该连接本就不持有控制权，不应注入。
pub fn on_release(conn: i32) -> bool {
    if !enabled() {
        return true;
    }
    let mut s = state();
    if s.holder != Some(conn) {
        return false;
    }
    let now = Instant::now();
    s.cursor_held = false;
    s.restore_at = Some(now + RESTORE_DELAY);
    s.holder_deadline = Some(now + GRANT_TIMEOUT);
    true
}

/// 滚轮：是否有权滚动。返回 `false` 表示拒绝本次滚轮。
pub fn admit_wheel(conn: i32) -> bool {
    if !enabled() {
        return true;
    }
    let mut s = state();
    if s.cursor_held && s.holder != Some(conn) {
        return false;
    }
    if let Some(h) = s.holder {
        if h != conn && !s.cursor_held {
            // 键盘仍被别人持有，滚轮也要排队，避免两个租客同时在这一台机器上滚。
            return false;
        }
    }
    if s.owner_saved.is_none() {
        s.owner_saved = crate::get_cursor_pos();
    }
    s.holder = Some(conn);
    s.last_inject_at = Some(Instant::now());
    true
}

/// 滚轮前应把真实光标移到的坐标；`None` 表示无需移动。
pub fn wheel_target(conn: i32) -> Option<(i32, i32)> {
    if !enabled() {
        return None;
    }
    let s = state();
    s.tenants.get(&conn).map(|t| t.cursor)
}

/// 滚轮结束后安排归还。仅在光标未被持续占用时生效。
pub fn after_wheel() {
    if !enabled() {
        return;
    }
    let mut s = state();
    if !s.cursor_held {
        let now = Instant::now();
        s.restore_at = Some(now + RESTORE_DELAY);
        s.holder_deadline = Some(now + GRANT_TIMEOUT);
    }
}

/// 键盘闸门。只有当前持有者可以注入键盘。
pub fn keyboard_allowed(conn: i32) -> bool {
    if !enabled() {
        return true;
    }
    state().holder == Some(conn)
}

/// 连接建立时登记，断开时清理。
pub fn detach(conn: i32) {
    if !enabled() {
        return;
    }
    let mut s = state();
    s.tenants.remove(&conn);
    if s.holder == Some(conn) {
        s.holder = None;
        s.holder_deadline = None;
        if s.cursor_held {
            s.cursor_held = false;
            s.restore_at = Some(Instant::now());
        }
    }
}

/// 由 33ms 光标采样线程驱动：检测所有者活动、释放过期持有权、执行归还。
///
/// 不另起线程——归还通过 [`restore_at`] 在这里落地，避免每次滚轮都 spawn 一个线程。
pub fn poll(pos: Option<(i32, i32)>) {
    if !enabled() {
        return;
    }
    let now = Instant::now();
    let mut restore_to = None;
    {
        let mut s = state();
        let settling = s
            .last_inject_at
            .map(|t| now.saturating_duration_since(t) < INJECTION_SETTLE)
            .unwrap_or(false);

        let mut owner_active = false;

        if let Some(p) = pos {
            if let Some(prev) = s.owner_pos {
                if !settling
                    && ((p.0 - prev.0).abs() > OWNER_ACTIVE_DISTANCE
                        || (p.1 - prev.1).abs() > OWNER_ACTIVE_DISTANCE)
                {
                    owner_active = true;
                }
            }
            s.owner_pos = Some(p);
        }

        // GetLastInputInfo 覆盖"所有者停在原地点击/敲键盘"，光位移判据覆盖不到。
        if let Some(tick) = last_input_tick() {
            if let Some(prev) = s.last_owner_tick {
                if tick != prev && !settling {
                    owner_active = true;
                }
            }
            s.last_owner_tick = Some(tick);
        }

        if owner_active && s.holder.is_some() {
            s.holder = None;
            s.holder_deadline = None;
            s.cursor_held = false;
            restore_to = s.owner_saved.take();
        }

        if let Some(d) = s.holder_deadline {
            if now >= d {
                s.holder = None;
                s.holder_deadline = None;
            }
        }

        if let Some(t) = s.restore_at {
            if now >= t {
                s.restore_at = None;
                if s.owner_saved.is_some() {
                    restore_to = s.owner_saved.take();
                }
            }
        }
    }

    if let Some(p) = restore_to {
        crate::platform::windows::try_change_desktop();
        let mut en = Enigo::new();
        en.mouse_move_to(p.0, p.1);
    }
}

/// 系统级最后一次输入事件的时间戳。注入自身也会刷新它，调用方需过滤。
fn last_input_tick() -> Option<u32> {
    use winapi::um::winuser::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    let ok = unsafe { GetLastInputInfo(&mut info) };
    if ok == 0 {
        None
    } else {
        Some(info.dwTime)
    }
}
