use super::{
    server::{Ripple, EVENT_PROXY},
    win_linux::{create_font_face, draw_text},
    Cursor, CustomEvent,
};
use hbb_common::{anyhow::anyhow, log, ResultType};
use softbuffer::{Context, Surface};
use std::{collections::HashMap, num::NonZeroU32, sync::Arc, time::Instant};
use tao::{
    dpi::{PhysicalPosition, PhysicalSize},
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    platform::windows::WindowBuilderExtWindows,
    window::WindowBuilder,
};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, PixmapMut, Stroke, Transform};

pub(super) fn create_event_loop() -> ResultType<()> {
    let face = match create_font_face() {
        Ok(face) => Some(face),
        Err(err) => {
            log::error!("Failed to create font face: {}", err);
            None
        }
    };

    let event_loop = EventLoopBuilder::<(String, CustomEvent)>::with_user_event().build();
    let mut window_builder = WindowBuilder::new()
        .with_title("RustDesk whiteboard")
        .with_transparent(true)
        .with_always_on_top(true)
        .with_skip_taskbar(true)
        .with_decorations(false);

    let mut final_size = None;
    // 虚拟桌面可能不在 (0,0) 起算（多显示器时常见负原点）。覆盖窗按这个原点摆放，
    // 而收到的光标坐标是桌面绝对坐标，绘制前必须减掉原点，否则会整体偏移。
    let mut origin = (0i32, 0i32);
    if let Ok((x, y, w, h)) = super::server::get_displays_rect() {
        if w > 0 && h > 0 {
            final_size = Some(PhysicalSize::new(w, h));
            origin = (x, y);
            window_builder = window_builder
                .with_position(PhysicalPosition::new(x, y))
                .with_inner_size(PhysicalSize::new(1, 1));
        } else {
            window_builder =
                window_builder.with_fullscreen(Some(tao::window::Fullscreen::Borderless(None)));
        }
    } else {
        window_builder =
            window_builder.with_fullscreen(Some(tao::window::Fullscreen::Borderless(None)));
    }

    let window = Arc::new(window_builder.build::<(String, CustomEvent)>(&event_loop)?);
    window.set_ignore_cursor_events(true)?;

    let context = Context::new(window.clone()).map_err(|e| {
        log::error!("Failed to create context: {}", e);
        anyhow!(e.to_string())
    })?;
    let mut surface = Surface::new(&context, window.clone()).map_err(|e| {
        log::error!("Failed to create surface: {}", e);
        anyhow!(e.to_string())
    })?;

    let proxy = event_loop.create_proxy();
    EVENT_PROXY.write().unwrap().replace(proxy);
    let _call_on_ret = crate::common::SimpleCallOnReturn {
        b: true,
        f: Box::new(move || {
            let _ = EVENT_PROXY.write().unwrap().take();
        }),
    };

    let mut ripples: Vec<Ripple> = Vec::new();
    let mut last_cursors: HashMap<String, Cursor> = HashMap::new();
    let mut resized = final_size.is_none();
    // 初始模式直接读被控端配置：IPC 首帧可能早于 collab::push_mode 的推送到达，
    // 只依赖推送的话角标会停在默认值上，显示成错误的模式。
    let mut collab_mode = {
        let v = hbb_common::config::Config::get_option("collab-mode");
        v == "Y" || v == "1" || v.eq_ignore_ascii_case("true")
    };

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Poll;

        match event {
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => {
                    *control_flow = ControlFlow::Exit;
                }
                _ => {}
            },
            Event::RedrawRequested(_) => {
                if !resized {
                    if let Some(size) = final_size.take() {
                        window.set_inner_size(size);
                    }
                    resized = true;
                    return;
                }

                let (width, height) = {
                    let size = window.inner_size();
                    (size.width, size.height)
                };

                let (Some(width), Some(height)) = (NonZeroU32::new(width), NonZeroU32::new(height))
                else {
                    return;
                };
                if let Err(e) = surface.resize(width, height) {
                    log::error!("Failed to resize surface: {}", e);
                    return;
                }

                let mut buffer = match surface.buffer_mut() {
                    Ok(buf) => buf,
                    Err(e) => {
                        log::error!("Failed to get buffer: {}", e);
                        return;
                    }
                };
                let Some(mut pixmap) = PixmapMut::from_bytes(
                    bytemuck::cast_slice_mut(&mut buffer),
                    width.get(),
                    height.get(),
                ) else {
                    log::error!("Failed to create pixmap from buffer");
                    return;
                };
                pixmap.fill(Color::TRANSPARENT);

                Ripple::retain_active(&mut ripples);
                for ripple in &ripples {
                    let (radius, alpha) = ripple.get_radius_alpha();

                    let mut ripple_paint = Paint::default();
                    // Note: The real color is bgra here.
                    ripple_paint.set_color_rgba8(64, 64, 255, (alpha * 128.0) as u8);
                    ripple_paint.anti_alias = true;

                    let mut ripple_pb = PathBuilder::new();
                    ripple_pb.push_circle(
                        ripple.x - origin.0 as f32,
                        ripple.y - origin.1 as f32,
                        radius,
                    );
                    if let Some(path) = ripple_pb.finish() {
                        pixmap.fill_path(
                            &path,
                            &ripple_paint,
                            FillRule::Winding,
                            Transform::identity(),
                            None,
                        );
                    }
                }

                for cursor in last_cursors.values() {
                    let (x, y) = (
                        cursor.x - origin.0 as f32,
                        cursor.y - origin.1 as f32,
                    );
                    let size = 1.5f32;

                    let mut pb = PathBuilder::new();
                    pb.move_to(x, y);
                    pb.line_to(x, y + 16.0 * size);
                    pb.line_to(x + 4.0 * size, y + 13.0 * size);
                    pb.line_to(x + 7.0 * size, y + 20.0 * size);
                    pb.line_to(x + 9.0 * size, y + 19.0 * size);
                    pb.line_to(x + 6.0 * size, y + 12.0 * size);
                    pb.line_to(x + 11.0 * size, y + 12.0 * size);
                    pb.close();

                    if let Some(path) = pb.finish() {
                        let rgba = super::argb_to_rgba(cursor.argb);
                        let mut arrow_paint = Paint::default();
                        // Note: The real color is bgra here.
                        arrow_paint.set_color_rgba8(rgba.2, rgba.1, rgba.0, rgba.3);
                        arrow_paint.anti_alias = true;
                        pixmap.fill_path(
                            &path,
                            &arrow_paint,
                            FillRule::Winding,
                            Transform::identity(),
                            None,
                        );

                        let mut black_paint = Paint::default();
                        black_paint.set_color_rgba8(0, 0, 0, 255);
                        black_paint.anti_alias = true;
                        let mut stroke = Stroke::default();
                        stroke.width = 1.0f32;
                        pixmap.stroke_path(
                            &path,
                            &black_paint,
                            &stroke,
                            Transform::identity(),
                            None,
                        );

                        face.as_ref().map(|face| {
                            draw_text(
                                &mut pixmap,
                                face,
                                &cursor.text,
                                x + 24.0 * size,
                                y + 24.0 * size,
                                &arrow_paint,
                                14.0f32,
                            );
                        });
                    }
                }

                // 每块屏幕右上角都画一个模式角标：视频流是按显示器单独编码的，
                // 只画主屏的话，共享别的屏时控制端就看不到。
                if let Some(face) = face.as_ref() {
                    let mut badge_paint = Paint::default();
                    badge_paint.set_color_rgba8(32, 32, 32, 235);
                    badge_paint.anti_alias = true;
                    let label = if collab_mode {
                        "协作模式 · 点击才生效"
                    } else {
                        "普通模式 · 双方均可控制"
                    };
                    for (dx, dy, dw, _dh) in super::server::get_displays() {
                        let right = (dx + dw as i32) - origin.0;
                        let top = dy - origin.1;
                        draw_text(
                            &mut pixmap,
                            face,
                            label,
                            right as f32 - 300.0,
                            top as f32 + 30.0,
                            &badge_paint,
                            20.0f32,
                        );
                    }
                }

                if let Err(e) = buffer.present() {
                    log::error!("Failed to present surface: {}", e);
                    return;
                }
            }
            Event::MainEventsCleared => {
                window.request_redraw();
            }
            Event::UserEvent((k, evt)) => match evt {
                CustomEvent::Cursor(cursor) => {
                    if cursor.btns != 0 {
                        ripples.push(Ripple {
                            x: cursor.x,
                            y: cursor.y,
                            start_time: Instant::now(),
                        });
                    }
                    last_cursors.insert(k, cursor);
                }
                CustomEvent::Mode(c) => {
                    collab_mode = c;
                }
                CustomEvent::Exit => {
                    *control_flow = ControlFlow::Exit;
                }
                _ => {}
            },
            _ => (),
        }
    });
}
