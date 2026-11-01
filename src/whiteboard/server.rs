use super::CustomEvent;
use crate::ipc::{new_listener, Connection, Data};
#[cfg(any(target_os = "windows", target_os = "macos"))]
use hbb_common::tokio::sync::mpsc::unbounded_channel;
#[cfg(any(target_os = "windows", target_os = "linux"))]
use hbb_common::ResultType;
use hbb_common::{
    allow_err, log,
    tokio::{self, sync::mpsc::UnboundedReceiver},
};
use lazy_static::lazy_static;
use std::sync::RwLock;
use std::time::{Duration, Instant};

#[cfg(any(target_os = "windows", target_os = "macos"))]
use tao::event_loop::EventLoopProxy;
#[cfg(target_os = "linux")]
use winit::event_loop::EventLoopProxy;

lazy_static! {
    pub(super) static ref EVENT_PROXY: RwLock<Option<EventLoopProxy<(String, CustomEvent)>>> =
        RwLock::new(None);
    /// 活着的 IPC 连接数。只有全部断开才退出白板进程，
    /// 否则任一连接关闭都会把别人的覆盖层一起带走。
    static ref ACTIVE_STREAMS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
}

const RIPPLE_DURATION: Duration = Duration::from_millis(500);
#[cfg(target_os = "macos")]
type RippleFloat = f64;
#[cfg(any(target_os = "windows", target_os = "linux"))]
type RippleFloat = f32;

#[cfg(target_os = "linux")]
pub use super::linux::run;

#[cfg(any(target_os = "windows", target_os = "macos"))]
pub fn run() {
    let (tx_exit, rx_exit) = unbounded_channel();
    std::thread::spawn(move || {
        start_ipc(rx_exit);
    });
    if let Err(e) = super::create_event_loop() {
        log::error!("Failed to create event loop: {}", e);
        tx_exit.send(()).ok();
        return;
    }
}

#[tokio::main(flavor = "current_thread")]
pub(super) async fn start_ipc(mut rx_exit: UnboundedReceiver<()>) {
    match new_listener("_whiteboard").await {
        Ok(mut incoming) => loop {
            tokio::select! {
                _ = rx_exit.recv() => {
                    log::info!("Exiting IPC");
                    break;
                }
                res = incoming.next() => match res {
                    Some(result) => match result {
                        Ok(stream) => {
                            log::debug!("Got new connection");
                            ACTIVE_STREAMS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            tokio::spawn(handle_new_stream(Connection::new(stream)));
                        }
                        Err(err) => {
                            log::error!("Couldn't get whiteboard client: {:?}", err);
                        }
                    },
                    None => {
                        log::error!("Failed to get whiteboard client");
                    }
                }
            }
        },
        Err(err) => {
            log::error!("Failed to start whiteboard ipc server: {}", err);
        }
    }
}

async fn handle_new_stream(mut conn: Connection) {
    loop {
        tokio::select! {
            res = conn.next() => {
                match res {
                    Err(err) => {
                        log::info!("whiteboard ipc connection closed: {}", err);
                        break;
                    }
                    Ok(Some(data)) => {
                        match data {
                            Data::Whiteboard((k, evt)) => {
                                if matches!(evt, CustomEvent::Exit) {
                                    log::info!("whiteboard ipc connection closed");
                                    break;
                                } else {
                                    EVENT_PROXY.read().unwrap().as_ref().map(|ep| {
                                        allow_err!(ep.send_event((k, evt)));
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                    Ok(None) => {
                        log::info!("whiteboard ipc connection closed");
                        break;
                    }
                }
            }
        }
    }
    let remaining = ACTIVE_STREAMS
        .fetch_sub(1, std::sync::atomic::Ordering::SeqCst)
        .saturating_sub(1);
    if remaining == 0 {
        EVENT_PROXY.read().unwrap().as_ref().map(|ep| {
            allow_err!(ep.send_event(("".to_string(), CustomEvent::Exit)));
        });
    }
}

/// 枚举每块显示器的矩形，供角标逐屏绘制。
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(super) fn get_displays() -> Vec<(i32, i32, u32, u32)> {
    let mut rects = Vec::new();
    if let Ok(displays) = crate::server::display_service::try_get_displays() {
        for d in displays {
            rects.push((
                d.origin().0 as i32,
                d.origin().1 as i32,
                d.width() as u32,
                d.height() as u32,
            ));
        }
    }
    rects
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(super) fn get_displays_rect() -> ResultType<(i32, i32, u32, u32)> {
    let displays = crate::server::display_service::try_get_displays()?;
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;

    for display in displays {
        let (x, y) = (display.origin().0 as i32, display.origin().1 as i32);
        let (w, h) = (display.width() as i32, display.height() as i32);
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x + w);
        max_y = max_y.max(y + h);
    }
    let (x, y) = (min_x, min_y);
    let (w, h) = ((max_x - min_x) as u32, (max_y - min_y) as u32);
    Ok((x, y, w, h))
}

#[inline]
pub(super) fn argb_to_rgba(argb: u32) -> (u8, u8, u8, u8) {
    (
        (argb >> 16 & 0xFF) as u8,
        (argb >> 8 & 0xFF) as u8,
        (argb & 0xFF) as u8,
        (argb >> 24 & 0xFF) as u8,
    )
}

pub(super) struct Ripple {
    pub x: RippleFloat,
    pub y: RippleFloat,
    pub start_time: Instant,
}

impl Ripple {
    #[inline]
    pub fn retain_active(ripples: &mut Vec<Ripple>) {
        ripples.retain(|r| r.start_time.elapsed() < RIPPLE_DURATION);
    }

    pub fn get_radius_alpha(&self) -> (RippleFloat, RippleFloat) {
        let elapsed = self.start_time.elapsed();
        #[cfg(target_os = "macos")]
        let progress = (elapsed.as_secs_f64() / RIPPLE_DURATION.as_secs_f64()).min(1.0);
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        let progress = (elapsed.as_secs_f32() / RIPPLE_DURATION.as_secs_f32()).min(1.0);
        #[cfg(target_os = "macos")]
        let radius = 25.0 * progress;
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        let radius = 45.0 * progress;
        let alpha = 1.0 - progress;
        (radius, alpha)
    }
}
