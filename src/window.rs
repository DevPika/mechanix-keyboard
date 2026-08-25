use std::time::Instant;

use ui::Point;
use utils::Rect;
use utils::Size;
use wayland::*;
use xkbcommon::xkb;

use crate::MechanixKeyboardState;
use crate::layout::MARGIN;
use crate::render;

const HEIGHT: u32 = 100;

#[derive(Default)]
pub struct WaylandGlobals {
    pub compositor: Option<Handle<WlCompositor>>,
    pub output: Option<Handle<WlOutput>>,
    pub layer_shell: Option<Handle<ZwlrLayerShellV1>>,
    pub dmabuf: Option<Handle<ZwpLinuxDmabufV1>>,
    pub seat: Option<Handle<WlSeat>>,
    pub pointer: Option<Handle<WlPointer>>,
    pub keyboard: Option<Handle<WlKeyboard>>,
    pub touch: Option<Handle<WlTouch>>,
    pub virtual_keyboard_manager: Option<Handle<ZwpVirtualKeyboardManagerV1>>,
    pub virtual_keyboard: Option<Handle<ZwpVirtualKeyboardV1>>,
}

pub struct WindowState {
    pub surface: Handle<WlSurface>,
    pub layer_surface: Handle<ZwlrLayerSurfaceV1>,
    pub slots: Option<[render::Slot; 2]>,
    pub back: usize,
    pub width: u32,
    pub height: u32,
    /// A frame callback fired while the back buffer was still in flight; draw as
    /// soon as its `wl_buffer.release` lands.
    pub pending_frame: bool,
}

/// The window + input module: registry/seat binding, layer-surface lifecycle,
/// and seat input into the interactivity crate.
pub fn module<S>() -> impl app::RegisteredModule<MechanixKeyboardState, S> {
    app::Module::new()
        .on(on_start)
        .on(on_pre_poll)
        .on(on_registry)
        .on(on_seat)
        .on(on_callback)
        .on(on_configure)
        .on(on_buffer_release)
        .on(on_keyboard)
        .on(on_pointer)
        .on(on_touch)
}

/// Kick off the registry roundtrip that discovers the globals.
fn on_start(s: &mut MechanixKeyboardState, _: &app::Start) {
    s.wayland.display().get_registry();
    s.wayland.display().sync();
}

/// Push queued requests to the compositor each poll.
fn on_pre_poll(s: &mut MechanixKeyboardState, _: &app::PrePoll) {
    s.wayland.proxy().flush();
}

/// Bind the globals the bar needs as the registry advertises them.
fn on_registry(s: &mut MechanixKeyboardState, event: &WlRegistryEvent) {
    let WlRegistryEvent::Global {
        sender,
        name,
        interface,
        version,
    } = event
    else {
        return;
    };
    match interface.as_str() {
        WlCompositor::NAME => s.globals.compositor = Some(sender.bind(*name, *version)),
        ZwlrLayerShellV1::NAME => s.globals.layer_shell = Some(sender.bind(*name, *version)),
        WlOutput::NAME => s.globals.output = Some(sender.bind(*name, *version)),
        ZwpLinuxDmabufV1::NAME => s.globals.dmabuf = Some(sender.bind(*name, *version)),
        WlSeat::NAME => s.globals.seat = Some(sender.bind(*name, *version)),
        ZwpVirtualKeyboardManagerV1::NAME => {
            s.globals.virtual_keyboard_manager = Some(sender.bind(*name, *version))
        }
        _ => {}
    }
}

/// Bind keyboard/pointer/touch as the seat reports having them.
fn on_seat(s: &mut MechanixKeyboardState, event: &WlSeatEvent) {
    let WlSeatEvent::Capabilities { capabilities, .. } = event else {
        return;
    };
    let Some(seat) = s.globals.seat.clone() else {
        return;
    };
    if capabilities.contains(WlSeatCapability::Keyboard) && s.globals.keyboard.is_none() {
        s.globals.keyboard = Some(seat.get_keyboard());
    }
    if capabilities.contains(WlSeatCapability::Pointer) && s.globals.pointer.is_none() {
        s.globals.pointer = Some(seat.get_pointer());
    }
    if capabilities.contains(WlSeatCapability::Touch) && s.globals.touch.is_none() {
        s.globals.touch = Some(seat.get_touch());
    }
}

/// Registry roundtrip done: globals are in, so create the layer surface and
/// commit it (no buffer yet — that waits for the first `configure`).
fn create_window(s: &mut MechanixKeyboardState) {
    if s.window.is_some() {
        return;
    }
    let (Some(compositor), Some(layer_shell)) = (&s.globals.compositor, &s.globals.layer_shell)
    else {
        return;
    };

    let surface = compositor.create_surface();
    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        None,
        ZwlrLayerShellV1Layer::Top,
        "mechanix-keyboard",
    );
    layer_surface.set_size(0, HEIGHT);
    layer_surface.set_anchor(
        ZwlrLayerSurfaceV1Anchor::Bottom
            | ZwlrLayerSurfaceV1Anchor::Left
            | ZwlrLayerSurfaceV1Anchor::Right,
    );
    layer_surface.set_exclusive_zone(0);
    layer_surface.set_keyboard_interactivity(ZwlrLayerSurfaceV1KeyboardInteractivity::None);
    surface.commit();

    s.window = Some(WindowState {
        surface,
        layer_surface,
        slots: None,
        back: 0,
        width: 0,
        height: HEIGHT,
        pending_frame: false,
    });
}

/// One `wl_callback.done`: either a frame callback we requested (repaint) or the
/// initial registry roundtrip (create the surface).
fn on_callback(s: &mut MechanixKeyboardState, event: &WlCallbackEvent) {
    let WlCallbackEvent::Done { sender, .. } = event;
    let Some(id) = sender.object_id() else {
        return;
    };
    if s.frame_callbacks.remove(&id) {
        // Frame callback: repaint (static colour today, ready for a live UI
        // once there are keys to draw).
        render::render(s);
    } else {
        create_window(s);
    }
}

/// Compositor sized the surface: ack, allocate slots on the first configure, and
/// present the first frame (which maps the surface).
fn on_configure(s: &mut MechanixKeyboardState, event: &ZwlrLayerSurfaceV1Event) {
    let ZwlrLayerSurfaceV1Event::Configure {
        serial,
        width,
        height,
        ..
    } = event
    else {
        return;
    };
    let Some(dmabuf) = s.globals.dmabuf.clone() else {
        return;
    };

    let need_alloc = {
        let Some(window) = s.window.as_mut() else {
            return;
        };
        let w = if *width == 0 { window.width } else { *width };
        let h = if *height == 0 { window.height } else { *height };
        window.layer_surface.ack_configure(*serial);
        if window.slots.is_none() {
            window.width = w;
            window.height = h;
            true
        } else {
            false
        }
    };

    if need_alloc {
        let (w, h) = {
            let window = s.window.as_ref().expect("window exists");
            (window.width, window.height)
        };
        let slots = render::alloc_slots(&mut s.renderer, &dmabuf, w, h);
        s.window.as_mut().expect("window exists").slots = Some(slots);
    }

    render::render(s);
}

/// The compositor handed a buffer back; mark it drawable and service any frame
/// that was waiting on it.
fn on_buffer_release(s: &mut MechanixKeyboardState, event: &WlBufferEvent) {
    let WlBufferEvent::Release { sender } = event;
    let Some(id) = sender.object_id() else {
        return;
    };
    if let Some(slots) = s.window.as_mut().and_then(|w| w.slots.as_mut()) {
        for slot in slots.iter_mut() {
            if slot.buffer_id == id {
                slot.released = true;
            }
        }
    }
    if s.window.as_ref().map_or(false, |w| w.pending_frame) {
        render::render(s);
    }
}

// ── input → interactivity ──────────────────────────────────────────────────

fn on_keyboard(s: &mut MechanixKeyboardState, event: &WlKeyboardEvent) {
    s.interactivity.call_before_frame();
    s.interactivity.process_keyboard(event);
    tracing::debug!(
        just_pressed = ?s.interactivity.keyboard.just_pressed_keys(),
        just_released = ?s.interactivity.keyboard.just_released_keys(),
        modifiers = ?s.interactivity.keyboard.modifiers(),
        "keyboard input",
    );
}

fn on_pointer(s: &mut MechanixKeyboardState, event: &WlPointerEvent) {
    s.interactivity.call_before_frame();
    s.interactivity.process_pointer(event);
    // tracing::debug!(?event, "pointer input");
    let Some(vkbd) = s.globals.virtual_keyboard.clone() else {
        return;
    };
    if let Some(layout) = s.layout.as_ref() {
        let width: f32;
        let height: f32;
        if let Some(outline) = layout.outlines.get("default") {
            width = outline.width;
            height = outline.height;
        } else {
            tracing::warn!("Using fallback width and height");
            width = 20.0;
            height = 10.0;
        }
        for (_, rows) in &layout.views {
            let mut y: f32 = 0.0;
            for row in rows {
                let mut x: f32 = 0.0;
                for button in row.split_whitespace() {
                    let mut state = WlKeyboardKeyState::Pressed;
                    let button_rect = Rect {
                        origin: Point::new(x, y),
                        size: Size::new(width, height),
                    };
                    let mut is_key_event_needed = false;
                    if s.interactivity
                        .pointer
                        .just_pressed(interactivity::pointer::MouseButton::Left)
                        && button_rect.contains_point(s.interactivity.pointer.position())
                    {
                        is_key_event_needed = true;
                        state = WlKeyboardKeyState::Pressed;
                        tracing::info!("Pressed {button}");
                    } else if s
                        .interactivity
                        .pointer
                        .just_released(interactivity::pointer::MouseButton::Left)
                        && button_rect.contains_point(s.interactivity.pointer.position())
                    {
                        is_key_event_needed = true;
                        state = WlKeyboardKeyState::Released;
                        tracing::info!("Released {button}");
                    }
                    if is_key_event_needed {
                        let Some(char) = button.chars().next() else {
                            continue;
                        };
                        let keysym = xkb::utf32_to_keysym(char as u32);

                        let Some(&x11_keycode) =
                            s.virtual_keyboard_state.keysym_map.get(&keysym.raw())
                        else {
                            tracing::warn!("No keycode found for keysym 0x{:x}", keysym.raw());
                            continue;
                        };

                        let evdev_key = x11_keycode - 8;

                        vkbd.key(
                            (Instant::now() - s.virtual_keyboard_state.start_time).as_millis()
                                as u32,
                            evdev_key,
                            state.into(),
                        );
                    }
                    x += width + MARGIN;
                }
                y += height + MARGIN;
            }
        }
    }
}

fn on_touch(s: &mut MechanixKeyboardState, event: &WlTouchEvent) {
    s.interactivity.call_before_frame();
    s.interactivity.process_touch(event);
    tracing::debug!(?event, "touch input");
}
