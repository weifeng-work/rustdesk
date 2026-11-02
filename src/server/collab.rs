//! 协作模式（Collab Mode）—— 所有者 + 租客的控制权模型。
//!
//! 与「浏览模式（View Mode）」并列的第三种会话模式。开关存在被控端配置项
//! `collab-mode` 里（持久化），设计文档见仓库根目录 `COLLAB_MODE_DESIGN.md`。
//!
//! 三条不变量：
//!
//! 1. **移动不碰真光标**：租客移动只更新虚拟光标，由白板浮窗绘制。
//! 2. **点击必然碰真光标，但不归位**：按下时把光标挪到租客位置，抬起后**留在原地**。
//!    归位那一次额外移动正是误选中/闪烁的根源，所以不做。
//! 3. **所有者优先**：所有者一动键鼠，立即撤销租客的鼠标控制权。
//!
//! 键盘不设闸门——与原版一致，双方都可注入。键盘是刻意动作，靠协作默契而非权限。
//!
//! 虚拟光标画在被控端已有的 whiteboard 浮窗上，因此**控制端无需安装魔改版**。

use enigo::{Enigo, MouseButton, MouseControllable};
use hbb_common::{config::Config, log};
use lazy_static::lazy_static;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

const COLLAB_OPTION: &str = "collab-mode";

/// 所有者光标位移超过该像素数即视为"所有者在动"。
const OWNER_ACTIVE_DISTANCE: i32 = 5;
/// 自身注入后多久内的输入变化不算所有者活动。
/// 太短会把注入的抬起误判成所有者在操作，导致控制权被自己撤销、键盘失效。
const INJECTION_SETTLE: Duration = Duration::from_millis(200);

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
    /// 白板角标需要重新推送一次模式。
    badge_dirty: bool,
    tenants: HashMap<i32, Tenant>,
    /// 当前持有鼠标注入权的连接。
    holder: Option<i32>,
    /// 真实光标是否正被租客占用（按下到抬起之间）。
    cursor_held: bool,
    last_inject_at: Option<Instant>,
    last_owner_tick: Option<u32>,
    owner_pos: Option<(i32, i32)>,
}

impl Collab {
    fn new() -> Self {
        Self {
            inited: false,
            enabled: false,
            badge_dirty: true,
            tenants: HashMap::new(),
            holder: None,
            cursor_held: false,
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

fn option_to_bool(v: &str) -> bool {
    v == "Y" || v == "1" || v.eq_ignore_ascii_case("true")
}

/// 协作模式是否开启。首次调用时从配置读一次。
pub fn enabled() -> bool {
    let mut s = state();
    if !s.inited {
        s.inited = true;
        s.enabled = option_to_bool(&Config::get_option(COLLAB_OPTION));
        log::info!("collab mode enabled: {}", s.enabled);
    }
    s.enabled
}

/// 切换模式：写回配置（持久化）、释放按键、刷新角标。
pub fn set_mode(on: bool) {
    Config::set_option(
        COLLAB_OPTION.to_owned(),
        if on { "Y" } else { "N" }.to_owned(),
    );
    {
        let mut s = state();
        s.inited = true;
        s.enabled = on;
        s.badge_dirty = true;
        s.holder = None;
        s.cursor_held = false;
    }
    // 切模式时租客可能正按着键，必须补发抬起，否则所有者那边会一直处于按下状态。
    release_all_buttons();
    if on {
        // 运行中打开协作模式：普通模式下不启动浮窗，这里给已在线的连接补注册，
        // 否则角标和虚拟光标都不会出现，必须重连才行。
        let conns: Vec<i32> = state().tenants.keys().copied().collect();
        for conn in conns {
            crate::whiteboard::register_whiteboard(crate::whiteboard::get_key_cursor(conn));
        }
    }
    crate::whiteboard::push_mode(on);
    log::info!("collab mode switched to {}", on);
}

/// 连接建立后登记身份，并启动承载虚拟光标的 whiteboard 浮窗。
///
/// 幂等：同一个 conn 重复调用只会更新名字与颜色。
pub fn attach(conn: i32, name: String, argb: u32) {
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
        s.badge_dirty = true;
    }
    if newly && enabled() {
        crate::whiteboard::register_whiteboard(crate::whiteboard::get_key_cursor(conn));
        log::info!("collab: whiteboard registered for conn {}", conn);
    }
    sync_badge();
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
    sync_badge();
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
            // 单租客场景下几乎不会发生；多租客留待后续。
            return false;
        }
        s.holder = Some(conn);
        s.cursor_held = true;
        s.last_inject_at = Some(Instant::now());
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
///
/// 这里**不把光标挪回去**：归位那一次额外移动会在目标程序仍认为按钮按下时变成拖拽，
/// 造成误选中一片文字。
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
        s.cursor_held = false;
        s.holder = None;
        // 注入的抬起会刷新 GetLastInputInfo，必须重置沉降窗口起点。
        s.last_inject_at = Some(Instant::now());
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

/// 滚轮：是否有权滚动。
pub fn admit_wheel(conn: i32) -> bool {
    if !enabled() {
        return true;
    }
    let mut s = state();
    if s.cursor_held && s.holder != Some(conn) {
        return false;
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
    let pos = s.tenants.get(&conn).map(|t| t.cursor).unwrap_or(NO_POS);
    if pos == NO_POS {
        None
    } else {
        Some(pos)
    }
}

/// 滚轮结束后释放鼠标控制权。光标不归位。
pub fn after_wheel() {
    if !enabled() {
        return;
    }
    let mut s = state();
    if !s.cursor_held {
        s.holder = None;
        s.last_inject_at = Some(Instant::now());
    }
}

/// 连接断开时清理，并关掉该连接对应的虚拟光标。
pub fn detach(conn: i32) {
    let registered;
    {
        let mut s = state();
        registered = s.tenants.remove(&conn).is_some();
        if s.holder == Some(conn) {
            s.holder = None;
            if s.cursor_held {
                s.cursor_held = false;
                release_all_buttons();
            }
        }
    }
    if registered {
        crate::whiteboard::unregister_whiteboard(crate::whiteboard::get_key_cursor(conn));
    }
}

/// 由 33ms 光标采样线程驱动：检测所有者活动，撤销租客的鼠标控制权。
pub fn poll(pos: Option<(i32, i32)>) {
    if !enabled() {
        return;
    }
    let now = Instant::now();
    let mut revoked_while_held = false;
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
            if s.cursor_held {
                s.cursor_held = false;
                revoked_while_held = true;
            }
        }
    }

    if revoked_while_held {
        // 中途夺回时必须补发抬起，否则所有者那边卡在按下状态。
        release_all_buttons();
    }
    sync_badge();
}

/// 把所有鼠标键盲发一次抬起。对没按下的键无害。
pub fn release_all_buttons() {
    crate::platform::windows::try_change_desktop();
    let mut en = Enigo::new();
    for b in [
        MouseButton::Left,
        MouseButton::Right,
        MouseButton::Middle,
        MouseButton::Back,
        MouseButton::Forward,
    ] {
        let _ = en.mouse_up(b);
    }
}

/// 模式角标变化时推给白板进程，成功送达才清脏标记。
fn sync_badge() {
    let dirty = state().badge_dirty;
    if !dirty {
        return;
    }
    let on = enabled();
    if crate::whiteboard::push_mode(on) {
        state().badge_dirty = false;
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
