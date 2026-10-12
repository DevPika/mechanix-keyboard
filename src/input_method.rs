//! `zwp_input_method_v2` — the input-method-v2 protocol client.
//!
//! `InputMethodState` is a `Resource`; the module binds `ZwpInputMethodManagerV2`
//! conditionally from `Globals`, creates the `ZwpInputMethodV2` object, and
//! registers a system for `ZwpInputMethodV2Event`.
//!
//! Inbound state is double-buffered: `activate`/`deactivate`/`content_type`
//! events fill a pending context that is applied on `done`. Surrounding text
//! and change cause are not tracked yet.

use mecha_wayland::prelude::*;

/// The text-input context the compositor reports for the focused field.
///
/// Inbound state is double-buffered: events fill `pending`, `Done` copies it
/// into `current`. Fields mirror the protocol's "initial values".
#[derive(Debug, Clone)]
pub struct InputMethodContext {
    /// `true` once `activate` has applied (on a `done`); `false` after
    /// `deactivate` applies. Drives whether taps route through IM2.
    pub active: bool,
    /// The focused field's content purpose; picks the view shown on activate.
    pub purpose: ZwpTextInputV3ContentPurpose,
}

impl Default for InputMethodContext {
    fn default() -> Self {
        Self {
            active: false,
            purpose: ZwpTextInputV3ContentPurpose::Normal,
        }
    }
}

/// All state the input-method-v2 client owns. Lives on the app as a `Resource`.
#[derive(Default)]
pub struct InputMethodState {
    /// The per-seat `zwp_input_method_v2` object; `None` until both the manager
    /// and a seat are available.
    pub input_method: Option<ZwpInputMethodV2>,

    /// The applied (post-`done`) inbound context — what the keyboard reads.
    pub current: InputMethodContext,
    /// The pending inbound context — what events are filling this batch.
    pending: InputMethodContext,

    /// Number of `done` events received. This is the serial echoed back in
    /// `commit(serial)`.
    pub serial: u32,

    /// `true` after `unavailable` — the object is inert.
    pub inert: bool,
}

impl Resource for InputMethodState {}

impl InputMethodState {
    /// Whether text taps should commit through IM2 rather than the
    /// virtual-keyboard-v1 transport. True only when active and not inert.
    pub fn should_commit(&self) -> bool {
        !self.inert && self.input_method.is_some() && self.current.active
    }
}

// ── keyboard surface visibility ──────────────────────────────────────────
//
// The keyboard surface's shown/hidden state lives in a `Resource` so that
// both the inbound IM2 state machine (`activate`/`deactivate` applied at
// `done`) and the debug visibility bar flip the *same* flag through the
// *same* setter. A `Signal` (`KeyboardVisibilityChanged`) is turned into a
// broadcast `Event` (`ApplyKeyboardVisibility`) by a system, which the
// `Shell` widget subscribes to in order to set `Display::Hidden`/`Flex` on
// the keyboard subtree. This mirrors the `theme` crate's `ThemeChanged` →
// `ApplyTheme` bridge — the only generic resource→widget path the app core
// exposes — so the bar and activate/deactivate share one mechanism (DRY).

/// The keyboard surface's shown/hidden state. Driven by IM2
/// `activate`/`deactivate` (via `Done`) and force-toggled by the debug
/// visibility bar; both write through [`KeyboardVisibilityExt`].
#[derive(Debug, Clone, Copy, Default)]
pub struct KeyboardVisibility {
    pub visible: bool,
}
impl Resource for KeyboardVisibility {}

/// `Signal`: the visibility flag changed → fan out `ApplyKeyboardVisibility`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyboardVisibilityChanged;
impl Signal for KeyboardVisibilityChanged {}

/// `Event`: re-apply the current visibility to the surface. Broadcast to the
/// root and every descendant; the `Shell` widget handles it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplyKeyboardVisibility;
impl Event for ApplyKeyboardVisibility {}

/// The single visibility setter (DRY): IM2 `Done` and the debug bar both go
/// through here. Implemented for `App` (systems) and `Context` (widget
/// handlers) so the same call reads at every site — mirroring `theme`'s
/// `AppThemeExt` / `ContextThemeExt` pair.
pub trait KeyboardVisibilityExt {
    /// The current visibility flag.
    fn keyboard_visible(&self) -> bool;
    /// Set the flag and signal the change (no-op if already `visible`).
    fn set_keyboard_visibility(&mut self, visible: bool);
    /// Flip the current visibility. Default method: every implementor gets it.
    fn toggle_keyboard_visibility(&mut self) {
        let next = !self.keyboard_visible();
        self.set_keyboard_visibility(next);
    }
}

impl KeyboardVisibilityExt for App {
    fn keyboard_visible(&self) -> bool {
        self.resource::<KeyboardVisibility>().visible
    }
    fn set_keyboard_visibility(&mut self, visible: bool) {
        if self.resource::<KeyboardVisibility>().visible == visible {
            return;
        }
        self.resource_mut::<KeyboardVisibility>().visible = visible;
        self.signal(KeyboardVisibilityChanged);
        tracing::info!(visible, "keyboard visibility");
    }
}

impl<W: Widget> KeyboardVisibilityExt for Context<'_, W> {
    fn keyboard_visible(&self) -> bool {
        self.resource::<KeyboardVisibility>().visible
    }
    fn set_keyboard_visibility(&mut self, visible: bool) {
        if self.resource::<KeyboardVisibility>().visible == visible {
            return;
        }
        self.resource_mut::<KeyboardVisibility>().visible = visible;
        self.signal(KeyboardVisibilityChanged);
        tracing::info!(visible, "keyboard visibility");
    }
}

pub struct SwitchView {
    pub view: &'static str, // view to switch to
}
impl Event for SwitchView {}

/// System: on `KeyboardVisibilityChanged`, broadcast `ApplyKeyboardVisibility`
/// to the root and every descendant so the `Shell` handler re-applies
/// `Display`. Mirrors `theme::on_theme_changed`.
fn on_keyboard_visibility_changed(app: &mut App, _: &KeyboardVisibilityChanged) {
    let mut targets = vec![app.root()];
    targets.extend(app.tree().descendants(app.root()));
    app.emit(ApplyKeyboardVisibility, targets);
}

/// Module that binds the input-method manager global, creates the IM object,
/// and handles `ZwpInputMethodV2Event`. Install after `WaylandModule`.
pub struct InputMethodModule;

impl Module for InputMethodModule {
    fn install(self, app: &mut App) {
        app.insert_resource(InputMethodState::default());
        // Keyboard surface visibility — starts hidden; IM2 `activate` and the
        // debug visibility bar flip it through `KeyboardVisibilityExt`.
        app.insert_resource(KeyboardVisibility { visible: false });
        app.system(on_keyboard_visibility_changed);
        init(app);
        app.system(on_input_method_event);
    }
}

/// Try to bind the input-method manager and create the IM object.
fn init(app: &mut App) {
    if app.resource::<InputMethodState>().input_method.is_some()
        || app.resource::<InputMethodState>().inert
    {
        return;
    }

    let seat = *app.resource::<WlSeat>();

    // The manager is an optional global — not every compositor supports
    // input-method-v2. Bind it from `Globals` if advertised.
    let manager = app
        .resource::<Globals>()
        .find(ZwpInputMethodManagerV2::NAME)
        .cloned()
        .map(|g| {
            let (globals, mut wl) = app.query::<(Res<Globals>, ResMut<Wayland>)>();
            globals.bind::<ZwpInputMethodManagerV2>(&g, &mut wl)
        });

    let Some(manager) = manager else {
        tracing::warn!("compositor does not advertise zwp_input_method_manager_v2");
        return;
    };

    let im = {
        let mut wl = app.resource_mut::<Wayland>();
        manager.get_input_method(&mut wl, seat)
    };

    tracing::info!("input-method-v2 bound");
    app.resource_mut::<InputMethodState>().input_method = Some(im);
}

/// Handle the inbound double-buffered state machine: events fill `pending`,
/// `done` applies it to `current` and bumps the serial, `unavailable` marks the
/// object inert.
fn on_input_method_event(app: &mut App, event: &ZwpInputMethodV2Event) {
    // store what we need to switch to if we get a content_type event
    let mut switch_to: Option<&'static str> = None;

    // Visibility is driven from the applied (post-`done`) active state: it
    // is computed inside the `InputMethodState` borrow and applied after the
    // guard drops, so `set_keyboard_visibility` can take `&mut App`.
    let apply_visible: Option<bool> = {
        let st = &mut app.resource_mut::<InputMethodState>();
        if st.inert {
            return;
        }
        match event {
            ZwpInputMethodV2Event::Activate { .. } => {
                // Activate resets all prior inbound state then arms active.
                st.pending = InputMethodContext {
                    active: true,
                    ..Default::default()
                };
                tracing::info!("input-method: activate (pending)");
                None
            }
            ZwpInputMethodV2Event::Deactivate { .. } => {
                st.pending.active = false;
                tracing::info!("input-method: deactivate (pending)");
                None
            }
            ZwpInputMethodV2Event::SurroundingText { .. }
            | ZwpInputMethodV2Event::TextChangeCause { .. } => None,
            ZwpInputMethodV2Event::ContentType { purpose, .. } => {
                st.pending.purpose = *purpose;
                None
            }
            ZwpInputMethodV2Event::Done { .. } => {
                // Call a SwitchView event if the text box requests us for a
                // different keyboard

                // TODO: LLM did this boolean logic, probably can be simplified
                // check if the text box became active
                let became_active = !st.current.active && st.pending.active;
                // check if we have changed purpose
                let purpose_changed = st.current.purpose != st.pending.purpose;
                // only if we are actually active and we switched view, do the change
                let should_switch_view = st.pending.active && (became_active || purpose_changed);

                if should_switch_view {
                    switch_to = Some(match st.pending.purpose {
                        ZwpTextInputV3ContentPurpose::Terminal => "terminal",
                        ZwpTextInputV3ContentPurpose::Number
                        | ZwpTextInputV3ContentPurpose::Digits => "number",
                        _ => "base",
                    })
                }

                // simple copy
                st.current = st.pending.clone();
                st.serial = st.serial.wrapping_add(1);
                let active = st.current.active;
                let serial = st.serial;
                tracing::info!(
                    active = active,
                    serial = serial,
                    should_switch_view,
                    "input-method: state applied"
                );
                // The applied active state drives the surface: show on
                // activate, hide on deactivate.
                Some(active)
            }
            ZwpInputMethodV2Event::Unavailable { .. } => {
                st.inert = true;
                tracing::warn!("input-method: unavailable; object now inert");
                None
            }
        }
    };
    if let Some(view) = switch_to {
        let targets: Vec<NodeId> = app
            .widgets::<crate::Keyboard>()
            .map(|(id, _keyboard)| id)
            .collect();
        app.emit(SwitchView { view }, targets);
    }
    if let Some(visible) = apply_visible {
        app.set_keyboard_visibility(visible);
    }
}

// ── outbound edits ────────────────────────────────────────────────────────

// Text commit is done by `im_commit_text` in main.rs, which sends
// `commit_string` + `commit(serial)` over Wayland.
