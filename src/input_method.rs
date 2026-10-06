//! `zwp_input_method_v2` — the input-method-v2 protocol client.
//!
//! Rewritten for the new mecha-wayland `app` core: `InputMethodState` is a
//! `Resource` and the module binds `ZwpInputMethodManagerV2` conditionally from
//! `Globals`, creates the `ZwpInputMethodV2` object, and registers a system for
//! `ZwpInputMethodV2Event`.
//!
//! The double-buffered state machine (inbound `activate`/`deactivate`/`done` →
//! applied context; outbound staged `commit_string`/`preedit`/`delete` → flushed
//! by `commit(serial)`) is preserved from the original implementation.

use app::{App, Module, Res, ResMut, Resource};
use wayland::{
    Globals, Interface, Wayland, WlSeat, ZwpInputMethodManagerV2, ZwpInputMethodV2,
    ZwpInputMethodV2Event, ZwpTextInputV3ChangeCause, ZwpTextInputV3ContentHint,
    ZwpTextInputV3ContentPurpose,
};

/// The text-input context the compositor reports for the focused field.
///
/// Inbound state is double-buffered: events fill `pending`, `Done` copies it
/// into `current`. Fields mirror the protocol's "initial values".
#[derive(Debug, Clone)]
pub struct InputMethodContext {
    /// `true` once `activate` has applied (on a `done`); `false` after
    /// `deactivate` applies. Drives whether taps route through IM2.
    pub active: bool,
    /// The last reported surrounding text, cursor, and selection anchor.
    pub surrounding: SurroundingText,
    /// Why the surrounding text last changed. Initial value is `InputMethod`.
    pub change_cause: ZwpTextInputV3ChangeCause,
    /// The focused field's content hint + purpose.
    pub content_type: ContentType,
}

impl Default for InputMethodContext {
    fn default() -> Self {
        Self {
            active: false,
            surrounding: SurroundingText::default(),
            change_cause: ZwpTextInputV3ChangeCause::InputMethod,
            content_type: ContentType::default(),
        }
    }
}

/// Surrounding-text slice reported by the text input (excluding any preedit).
#[derive(Debug, Clone, Default)]
pub struct SurroundingText {
    pub text: String,
    pub cursor: u32,
    pub anchor: u32,
    /// `true` once a `surrounding_text` event has been seen in the pending batch.
    pub reported: bool,
}

/// Content type hint + purpose for the focused field.
#[derive(Debug, Clone, Copy)]
pub struct ContentType {
    pub hint: ZwpTextInputV3ContentHint,
    pub purpose: ZwpTextInputV3ContentPurpose,
}

impl Default for ContentType {
    fn default() -> Self {
        Self {
            hint: ZwpTextInputV3ContentHint::empty(),
            purpose: ZwpTextInputV3ContentPurpose::Normal,
        }
    }
}

/// One staged preedit string.
#[derive(Debug, Clone)]
pub struct Preedit {
    pub text: String,
    pub cursor_begin: i32,
    pub cursor_end: i32,
}

/// One staged surrounding-text deletion.
#[derive(Debug, Clone, Copy)]
pub struct DeleteSurrounding {
    pub before: u32,
    pub after: u32,
}

/// Outbound edits being staged for the next `commit`. Each field is `Option`,
/// distinguishing "explicitly set this commit" from "leave at initial value".
#[derive(Debug, Default)]
pub struct PendingEdit {
    pub commit_string: Option<String>,
    pub preedit: Option<Preedit>,
    pub delete: Option<DeleteSurrounding>,
}

/// All state the input-method-v2 client owns. Lives on the app as a `Resource`.
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

    /// Outbound edits staged since the last `commit`. Flushed by `flush`.
    pub pending_edit: PendingEdit,

    /// `true` after `unavailable` — the object is inert.
    pub inert: bool,
}

impl Default for InputMethodState {
    fn default() -> Self {
        Self {
            input_method: None,
            current: InputMethodContext {
                change_cause: ZwpTextInputV3ChangeCause::InputMethod,
                ..Default::default()
            },
            pending: InputMethodContext {
                change_cause: ZwpTextInputV3ChangeCause::InputMethod,
                ..Default::default()
            },
            serial: 0,
            pending_edit: PendingEdit::default(),
            inert: false,
        }
    }
}

impl Resource for InputMethodState {}

impl InputMethodState {
    /// Whether text taps should commit through IM2 rather than the
    /// virtual-keyboard-v1 transport. True only when active and not inert.
    pub fn should_commit(&self) -> bool {
        !self.inert && self.input_method.is_some() && self.current.active
    }

    /// Stage a `commit_string` for the next flush.
    pub fn stage_commit_string(&mut self, text: impl Into<String>) {
        if !self.inert && self.input_method.is_some() {
            self.pending_edit.commit_string = Some(text.into());
        }
    }
}

/// Module that binds the input-method manager global, creates the IM object,
/// and handles `ZwpInputMethodV2Event`. Install after `WaylandModule`.
pub struct InputMethodModule;

impl Module for InputMethodModule {
    fn install(self, app: &mut App) {
        app.insert_resource(InputMethodState::default());
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
    let st = &mut app.resource_mut::<InputMethodState>();
    if st.inert {
        return;
    }
    match event {
        ZwpInputMethodV2Event::Activate { .. } => {
            // Activate resets all prior inbound state then arms active.
            st.pending = InputMethodContext {
                active: true,
                change_cause: ZwpTextInputV3ChangeCause::InputMethod,
                ..Default::default()
            };
            tracing::info!("input-method: activate (pending)");
        }
        ZwpInputMethodV2Event::Deactivate { .. } => {
            st.pending.active = false;
            tracing::info!("input-method: deactivate (pending)");
        }
        ZwpInputMethodV2Event::SurroundingText {
            text,
            cursor,
            anchor,
            ..
        } => {
            st.pending.surrounding = SurroundingText {
                text: text.clone(),
                cursor: *cursor,
                anchor: *anchor,
                reported: true,
            };
        }
        ZwpInputMethodV2Event::TextChangeCause { cause, .. } => {
            st.pending.change_cause = *cause;
        }
        ZwpInputMethodV2Event::ContentType { hint, purpose, .. } => {
            st.pending.content_type = ContentType {
                hint: *hint,
                purpose: *purpose,
            };
        }
        ZwpInputMethodV2Event::Done { .. } => {
            st.current = st.pending.clone();
            st.serial = st.serial.wrapping_add(1);
            let active = st.current.active;
            let serial = st.serial;
            tracing::info!(
                active = active,
                serial = serial,
                "input-method: state applied"
            );
        }
        ZwpInputMethodV2Event::Unavailable { .. } => {
            st.inert = true;
            tracing::warn!("input-method: unavailable; object now inert");
        }
    }
}

// ── outbound edits ────────────────────────────────────────────────────────

// Text commit is handled in main.rs's `im_commit_text` function, which uses
// `Context`'s resource access to stage the commit string and flush it via
// Wayland. The `InputMethodState::stage_commit_string` method above provides
// the staging API.
