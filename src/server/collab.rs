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
//! 3. **点击才抢占**：移动只更新虚拟光标位置，按下才把真实光标瞬移到租客位置、抬起后归还。
//!
//! 虚拟光标画在被控端已有的 whiteboard 浮窗上，因此**不需要控制端也换魔改版**——
//! 官方客户端连进来一样能看到自己的彩色指针。这也是本模块不复用 `show_my_cursor`
//! 通道的原因：那条路被客户端绑死在浏览模式上，而浏览模式会让鼠标注入彻底失效。

use enigo::{Enigo, MouseControllable};
use hbb_common::log;
use lazy_static::lazy_static;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// 租客最后一次活动后，键盘所有权保留多久（移动、点击、打字都会续期）。
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// 抬起后归还真实光标前的缓冲，给目标程序留出处理 click 的时间。
const RESTORE_DELAY: Duration = Duration::from_millis(150);
/// 所有者光标位移超过该像素数即视为"所有者在动"。
const OWNER_ACTIVE_DISTANCE: i32 = 5;
/// 自身注入后多久内的输入变化不算所有者活动。
const INJECTION_SETTLE: Duration = Duration::from_millis(50);

const NO_POS: (i32, i32) = (i32::MIN, i32::MIN);

struct Tenant {
    /// 租客最新意图坐标。
    cursor: (i32, i32),
    /// 显示在虚拟光标旁的名字。
    name: String,
    /// 虚拟光标颜色，由连接身份派生，同一个人每次一致。
    argb: u32,
}

impl Tenant {
    fn new(name: String, argb: u32) -> Self {
        Self {
            cursor: NO_POS,
            name,
            argb,
        }
    }
}

struct Collab {
    inited: bool,
    enabled: bool,
    tenants: HashMap<i32, Tenant>,
    /// 键鼠注入权的持有者；`None` 表示无人持有。
    holder: Option<i32>,
    /// 持有者的过期时刻，租客有活动就续期。
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

/// 连接建立后登记身份，并启动承载虚拟光标的 whiteboard 浮窗。
///
/// 幂等：同一个 conn 重复调用只会更新名字与颜色。
pub fn attach(conn: i32, name: String, argb: u32) {
    if !enabled() {
        return;
    }
    let mut newly = false;
    {
        let mut s = state();
        if let Some(t) = s.tenants.get_mut(&conn) {
            if !name.is_empty() {
                t.name = name;
            }
            if argb != 0 {
                t.argb = argb;
            }
        } else {
            s.tenants.insert(conn, Tenant::new(name, argb));
            newly = true;
        }
        if let Some(t) = s.tenants.get_mut(&conn) {
            if t.cursor == NO_POS {
                t.cursor = crate::get_cursor_pos().unwrap_or(NO_POS);
            }
        }
    }
    if newly {
        crate::whiteboard::register_whiteboard(crate::whiteboard::get_key_cursor(conn));
        log::info!("collab: whiteboard registered for conn {}", conn);
    }
}

/// 鼠标移动。返回 `true` 表示调用方照旧注入；`false` 表示只记录坐标。
pub fn on_move(conn: i32, x: i32, y: i32) -> bool {
    if !enabled() {
        return true;
    }
    let now = Instant::now();
    let (inject, argb, name);
    {
        let mut s = state();
        if let Some(t) = s.tenants.get_mut(&conn) {
            t.cursor = (x, y);
        } else {
            s.tenants.insert(conn, Tenant::new(String::new(), 0));
            if let Some(t) = s.tenants.get_mut(&conn) {
                t.cursor = (x, y);
            }
        }
        if s.holder == Some(conn) {
            s.holder_deadline = Some(now + IDLE_TIMEOUT);
        }
        inject = s.cursor_held && s.holder == Some(conn);
        if inject {
            s.last_inject_at = Some(now);
        }
        match s.tenants.get(&conn) {
            Some(t) => {
                argb = t.argb;
                name = t.name.clone();
            }
            None => {
                argb = 0;
                name = String::new();
            }
        }
    }
    push_cursor(conn, x, y, argb, 0, name);
    inject
}

/// 鼠标按下。返回 `false` 表示拒绝本次注入。
pub fn admit_press(conn: i32) -> bool {
    if !enabled() {
        return true;
    }
    let mut publish = None;
    {
        let mut s = state();
        if s.cursor_held && s.holder != Some(conn) {
            return false;
        }
        if s.owner_saved.is_none() {
            s.owner_saved = crate::get_cursor_pos();
        }
        s.holder = Some(conn);
        s.holder_deadline = Some(Instant::now() + IDLE_TIMEOUT);
        s.cursor_held = true;
        s.restore_at = None;
        if let Some(t) = s.tenants.get(&conn) {
            if t.cursor != NO_POS {
                publish = Some((t.cursor.0, t.cursor.1, t.argb, t.name.clone()));
            }
        }
    }
    // btns != 0 会让白板画一次点击涟漪。
    if let Some((x, y, argb, name)) = publish {
        push_cursor(conn, x, y, argb, 1, name);
    }
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
    let target = s.tenants.get(&conn).map(|t| t.cursor).unwrap_or(NO_POS);
    if target == NO_POS {
        return None;
    }
    s.last_inject_at = Some(Instant::now());
    Some(target)
}

/// 鼠标抬起。返回 `false` 表示该连接本就不持有控制权。
pub fn on_release(conn: i32) -> bool {
    if !enabled() {
        return true;
    }
    let mut publish = None;
    {
        let mut s = state();
        if s.holder != Some(conn) {
            return false;
        }
        let now = Instant::now();
        s.cursor_held = false;
        s.restore_at = Some(now + RESTORE_DELAY);
        s.holder_deadline = Some(now + IDLE_TIMEOUT);
        if let Some(t) = s.tenants.get(&conn) {
            if t.cursor != NO_POS {
                publish = Some((t.cursor.0, t.cursor.1, t.argb, t.name.clone()));
            }
        }
    }
    if let Some((x, y, argb, name)) = publish {
        push_cursor(conn, x, y, argb, 0, name);
    }
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
            return false;
        }
    }
    if s.owner_saved.is_none() {
        s.owner_saved = crate::get_cursor_pos();
    }
    s.holder = Some(conn);
    s.holder_deadline = Some(Instant::now() + IDLE_TIMEOUT);
    s.last_inject_at = Some(Instant::now());
    true
}

/// 滚轮前应把真实光标移到的坐标；`None` 表示无需移动。
pub fn wheel_target(conn: i32) -> Option<(i32, i32)> {
    if !enabled() {
        return None;
    }
    let s = state();
    let pos = s.tenants.get(&conn).map(|t| t.cursor).unwrap_or(NO_POS);
    if pos == NO_POS {
        None
    } else {
        Some(pos)
    }
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
        s.holder_deadline = Some(now + IDLE_TIMEOUT);
    }
}

/// 键盘闸门。只有当前持有者可以注入键盘；放行时顺带续期。
pub fn keyboard_allowed(conn: i32) -> bool {
    if !enabled() {
        return true;
    }
    let mut s = state();
    if s.holder == Some(conn) {
        s.holder_deadline = Some(Instant::now() + IDLE_TIMEOUT);
        true
    } else {
        false
    }
}

/// 连接断开时清理，并关掉该连接对应的虚拟光标。
pub fn detach(conn: i32) {
    if !enabled() {
        return;
    }
    let registered;
    {
        let mut s = state();
        registered = s.tenants.remove(&conn).is_some();
        if s.holder == Some(conn) {
            s.holder = None;
            s.holder_deadline = None;
            if s.cursor_held {
                s.cursor_held = false;
                s.restore_at = Some(Instant::now());
            }
        }
    }
    if registered {
        crate::whiteboard::unregister_whiteboard(crate::whiteboard::get_key_cursor(conn));
    }
}

/// 由 33ms 光标采样线程驱动：检测所有者活动、释放过期持有权、执行归还。
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

/// 把租客的虚拟光标推给 whiteboard 浮窗。
fn push_cursor(conn: i32, x: i32, y: i32, argb: u32, btns: i32, name: String) {
    crate::whiteboard::update_whiteboard(
        crate::whiteboard::get_key_cursor(conn),
        crate::whiteboard::CustomEvent::Cursor(crate::whiteboard::Cursor {
            x: x as f32,
            y: y as f32,
            argb,
            btns,
            text: name,
        }),
    );
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
