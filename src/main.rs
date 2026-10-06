//! mechanix-keyboard — an on-screen keyboard built with themed widgets on the
//! new mecha-wayland UI core.
//!
//! This is the successor to the old standalone OSK that issued raw render
//! commands (`DrawRect` / `DrawText` / `DrawMonochromeSprite`) and managed its
//! own dmabuf slots. The new version builds a tree of themed `Widget` nodes:
//! each key is a `Key` widget whose background, label colour, and corner radius
//! come from `MechanixTheme` / `ColorRole`, and whose `on_theme` callback
//! re-resolves them instantly when the user toggles dark/light at runtime.
//!
//! The keymap (parsed from `resources/layout.yaml` by `layout.rs`) drives which
//! keys appear and what they do. View switching (Shift_L `locking`,
//! `show_symbols`, `show_eschars`) and one-shot modifier latching (Ctrl) are
//! preserved. Text keys commit through `zwp_input_method_v2` when a text input
//! is focused, falling back to `zwp_virtual_keyboard_v1` keysym transport.

#![recursion_limit = "1024"]

use mecha_wayland::prelude::*;
use app::Resource;

mod input_method;
mod layout;
mod virtual_keyboard;

use layout::{KeyAction, Keymap};

/// The keymap view shown when the keyboard first appears.
pub const INITIAL_VIEW: &str = "base";

/// Shared state for the Ctrl one-shot latch. Stored as a `Resource` so both
/// the Key widget's `on_theme` handler and the Keyboard's `dispatch` function
/// can read/write it.
#[derive(Default)]
struct LatchedState {
    ctrl: bool,
}
impl Resource for LatchedState {}

// ── Key widget (themed button) ────────────────────────────────────────────

/// What a key looks like — determines the `ColorRole` pair it draws with.
/// Derived from the key's `KeyAction` so the visual matches the behaviour.
#[derive(Clone, Copy)]
enum KeyKind {
    /// Regular character key (letters, digits, punctuation).
    Normal,
    /// Modifier / view-switch key (Shift, Ctrl, abc, Fn, …).
    Modifier,
    /// Action key (Enter, Backspace).
    Action,
    /// Space — wide, low-emphasis.
    Space,
}

/// Classify a key's action into a visual kind for theme colour selection.
fn kind_of(action: &KeyAction) -> KeyKind {
    match action {
        KeyAction::EmitKeysym(_) | KeyAction::EmitText(_) => KeyKind::Normal,
        KeyAction::LatchModifier(_) => KeyKind::Modifier,
        KeyAction::SetView(_) | KeyAction::ToggleView { .. } => KeyKind::Modifier,
        KeyAction::Unhandled(_) => KeyKind::Modifier,
    }
}

/// Resolve a `KeyKind` to its (background, foreground) colour roles. A latched
/// modifier uses the `Primary` palette so the armed state reads at a glance.
fn roles_for(kind: KeyKind, latched: bool) -> (ColorRole, ColorRole) {
    if latched {
        return (ColorRole::Primary, ColorRole::OnPrimary);
    }
    match kind {
        KeyKind::Normal => (ColorRole::SurfaceContainerHigh, ColorRole::OnSurface),
        KeyKind::Modifier => (ColorRole::SecondaryContainer, ColorRole::OnSecondaryContainer),
        KeyKind::Action => (ColorRole::PrimaryContainer, ColorRole::OnPrimaryContainer),
        KeyKind::Space => (ColorRole::SurfaceContainerHighest, ColorRole::OnSurfaceVariant),
    }
}

/// The flex-grow weight for a key, derived from its outline width relative to
/// the default 50px key. This preserves the original layout's key sizing.
fn grow_weight(key: &layout::Key) -> f32 {
    key.rect.w / 50.0
}

/// A themed key button: a `div` with a `Paint::Quad` background resolved from
/// `ColorRole`, a centred text label, and an `on_theme` callback that
/// re-resolves both colours when the theme mode flips at runtime.
///
/// The key's `Clicked` handler dispatches its `KeyAction` through the virtual
/// keyboard / input method / view-switch logic.
struct Key;

struct KeyBuilder {
    font: FontId,
    key: layout::Key,
    /// Index of the view this key belongs to, for the latched-state check.
    view_index: usize,
}

impl Build for KeyBuilder {
    type Widget = Key;
}

impl Widget for Key {
    type Builder = KeyBuilder;
    fn build(b: KeyBuilder, me: Handle<Self>, s: &mut Spawner<'_, Self>) -> Self {
        let kind = kind_of(&b.key.action);
        let (bg_role, fg_role) = roles_for(kind, false);

        *s.component_mut::<LayoutStyle>(me).unwrap() =
            LayoutStyle::default().center().grow(grow_weight(&b.key)).padding_all(px(2.0));

        *s.component_mut::<Paint>(me).unwrap() =
            Paint::Quad(Quad::new(s.color(bg_role)).radius(6.0));

        let label = s.spawn(
            me,
            text(b.font, b.key.display_label())
                .color(s.color(fg_role))
                .size(16),
        );

        // Re-resolve colours on theme change. A latched modifier key gets the
        // armed (Primary) palette instead of its normal one.
        let is_latch = matches!(b.key.action, KeyAction::LatchModifier(_));
        s.on_theme(me, move |ctx| {
            let latched = is_latch && ctx.resource::<LatchedState>().ctrl;
            let (bg_role, fg_role) = roles_for(kind, latched);
            let bg = ctx.color(bg_role);
            let fg = ctx.color(fg_role);
            ctx.set_paint(Paint::Quad(Quad::new(bg).radius(6.0)));
            ctx.at(label).unwrap().set_color(fg);
        });

        Key
    }
}

// ── Keyboard widget ───────────────────────────────────────────────────────

/// The on-screen keyboard: manages the current view, the Ctrl one-shot latch,
/// and spawns themed `Key` widgets from the parsed keymap. All views are
/// spawned at build time; non-current views are `Display::Hidden` and toggled
/// visible on view-switch key taps.
struct Keyboard {
    /// Index into the keymap's views of the view currently shown.
    current_view: usize,
    /// One `Handle<Div>` per view; toggled `Display::Hidden`/`Flex` on switch.
    view_nodes: Vec<Handle<Div>>,
    /// Every Ctrl key across all views, for armed-state repaint on latch toggle.
    ctrl_keys: Vec<Handle<Key>>,
    /// The keymap (kept for view name lookup on switch).
    keymap: Keymap,
}

struct KeyboardBuilder {
    font: FontId,
    keymap: Keymap,
}

impl Build for KeyboardBuilder {
    type Widget = Keyboard;
}

impl Widget for Keyboard {
    type Builder = KeyboardBuilder;
    fn build(b: KeyboardBuilder, me: Handle<Self>, s: &mut Spawner<'_, Self>) -> Self {
        // Keyboard container: column filling the window, surface background.
        *s.component_mut::<LayoutStyle>(me).unwrap() =
            LayoutStyle::default().column().fill().gap(px(4.0)).padding_all(px(4.0));
        *s.component_mut::<Paint>(me).unwrap() =
            Paint::Quad(Quad::new(s.color(ColorRole::Surface)));

        s.on_theme(me, move |ctx| {
            let bg = ctx.color(ColorRole::Surface);
            ctx.set_paint(Paint::Quad(Quad::new(bg)));
        });

        let initial_view = b.keymap.index_of(INITIAL_VIEW).unwrap_or(0);

        // ── spawn all views ─────────────────────────────────────────────
        let mut view_nodes = Vec::new();
        let mut ctrl_keys = Vec::new();

        for (vi, view) in b.keymap.views.iter().enumerate() {
            let view_style = if vi == initial_view {
                LayoutStyle::default().column().fill().gap(px(4.0))
            } else {
                LayoutStyle::default().column().fill().gap(px(4.0)).hidden()
            };
            let view_div = s.spawn(me, div().style(view_style));
            view_nodes.push(view_div);

            for row in &view.rows {
                let row_div = s.spawn(
                    view_div,
                    div().style(LayoutStyle::default().row().fill().gap(px(4.0))),
                );

                for key in &row.keys {
                    let is_latch = matches!(&key.action, KeyAction::LatchModifier(_));
                    let key_handle = s.spawn(
                        row_div,
                        KeyBuilder {
                            font: b.font,
                            key: key.clone(),
                            view_index: vi,
                        },
                    );

                    if is_latch {
                        ctrl_keys.push(key_handle);
                    }

                    // Clone the action into the Clicked closure. Each key
                    // gets its own handler that dispatches through the
                    // Keyboard's action logic.
                    let action = key.action.clone();
                    s.on::<Clicked>(key_handle, move |ctx, _| {
                        dispatch(ctx, &action);
                    });
                }
            }
        }

        Keyboard {
            current_view: initial_view,
            view_nodes,
            ctrl_keys,
            keymap: b.keymap,
        }
    }
}

// ── action dispatch ───────────────────────────────────────────────────────

/// Route a tapped key's action. View switches and modifier latches mutate the
/// Keyboard widget state; every other action is a keystroke the virtual
/// keyboard emits (consuming any latched modifier).
fn dispatch(ctx: &mut Context<'_, Keyboard>, action: &KeyAction) {
    match action {
        KeyAction::SetView(name) => {
            switch_view(ctx, name);
        }
        KeyAction::ToggleView { lock, unlock } => {
            let current = {
                let me = ctx.me();
                me.keymap.views[me.current_view].name.clone()
            };
            let target = if &current == lock { unlock } else { lock };
            switch_view(ctx, target);
        }
        KeyAction::LatchModifier(_) => {
            toggle_latch(ctx);
        }
        KeyAction::EmitKeysym(ks) => {
            let latched_mask = latched_mask(ctx);
            // Printable keysyms commit via IM2; control keysyms fall back to
            // the virtual-keyboard-v1 keysym transport.
            if let Some(text) = virtual_keyboard::keysym_text(*ks)
                && im_commit_text(ctx, &text)
            {
                tracing::info!(input = %text, "input method");
            } else {
                vk_emit_keysym(ctx, *ks, latched_mask);
            }
            consume_latch(ctx);
        }
        KeyAction::EmitText(text) => {
            let latched_mask = latched_mask(ctx);
            if im_commit_text(ctx, text) {
                tracing::info!("Input method: {text}");
            } else {
                for ch in text.chars() {
                    vk_emit_keysym(ctx, xkbcommon::xkb::Keysym::from_char(ch), latched_mask);
                }
            }
            consume_latch(ctx);
        }
        KeyAction::Unhandled(name) => {
            tracing::info!(action = %name, "tapped key with no wired action");
        }
    }
}

/// The combined modifier mask of any currently-latched Ctrl, to OR into a
/// keystroke. Reads from the `LatchedState` resource.
fn latched_mask(ctx: &Context<'_, Keyboard>) -> u32 {
    if !ctx.resource::<LatchedState>().ctrl {
        return 0;
    }
    ctx.resource::<virtual_keyboard::VirtualKeyboardState>()
        .mod_masks
        .get("Control")
        .copied()
        .unwrap_or(0)
}

/// Try to commit text through `zwp_input_method_v2`. Returns `true` if the edit
/// was sent (IM bound + active). Uses `Context`'s resource access — borrows each
/// resource sequentially to avoid conflicts.
fn im_commit_text(ctx: &mut Context<'_, Keyboard>, text: &str) -> bool {
    if !ctx.resource::<input_method::InputMethodState>().should_commit() {
        return false;
    }
    // Stage the commit string + copy out the IM object (Copy) and serial.
    let (im_obj, serial) = {
        let st = &mut ctx.resource_mut::<input_method::InputMethodState>();
        st.stage_commit_string(text);
        (st.input_method, st.serial)
    };
    let Some(im) = im_obj else { return false; };
    // Send the requests via Wayland.
    let mut wl = ctx.resource_mut::<Wayland>();
    im.commit_string(&mut wl, text);
    im.commit(&mut wl, serial);
    true
}

/// Send one keysym as a keycode down+up via `zwp_virtual_keyboard_v1`, holding
/// the keystroke's modifiers around it and clearing them after.
fn vk_emit_keysym(ctx: &mut Context<'_, Keyboard>, ks: xkbcommon::xkb::Keysym, latched_mask: u32) {
    let name = xkbcommon::xkb::keysym_get_name(ks);
    // Copy out the keystroke + VK handle (all Copy) from the shared resource.
    let (vkbd, stroke) = {
        let vk = ctx.resource::<virtual_keyboard::VirtualKeyboardState>();
        (vk.virtual_keyboard, vk.keycodes.get(&ks).copied())
    };
    let Some(vkbd) = vkbd else {
        tracing::warn!(keysym = %name, "virtual keyboard not ready; key dropped");
        return;
    };
    let Some(stroke) = stroke else {
        tracing::warn!(keysym = %name, "keysym absent from keymap; not typed");
        return;
    };
    let mods = stroke.mods | latched_mask;
    let time = (std::time::Instant::now()
        - ctx.resource::<virtual_keyboard::VirtualKeyboardState>().start_time)
        .as_millis() as u32;
    let mut wl = ctx.resource_mut::<Wayland>();
    if mods != 0 {
        vkbd.modifiers(&mut wl, mods, 0, 0, 0);
    }
    vkbd.key(&mut wl, time, stroke.code, u32::from(WlKeyboardKeyState::Pressed));
    vkbd.key(&mut wl, time, stroke.code, u32::from(WlKeyboardKeyState::Released));
    if mods != 0 {
        vkbd.modifiers(&mut wl, 0, 0, 0, 0);
    }
    tracing::info!(keysym = %name, code = stroke.code, mods, "typed");
}

/// Clear the Ctrl latch after a keystroke fires (one-shot).
fn consume_latch(ctx: &mut Context<'_, Keyboard>) {
    if ctx.resource::<LatchedState>().ctrl {
        ctx.resource_mut::<LatchedState>().ctrl = false;
        repaint_ctrl_keys(ctx);
    }
}

/// Switch the current view to the named one, hiding the old and showing the
/// new via `Display::Hidden` / `Display::Flex`.
fn switch_view(ctx: &mut Context<'_, Keyboard>, target: &str) {
    let Some(idx) = ctx.me().keymap.index_of(target) else {
        tracing::warn!(view = %target, "view switch to unknown view; ignored");
        return;
    };
    let old_idx = ctx.me().current_view;
    if idx == old_idx {
        return;
    }
    let (old, new) = {
        let me = ctx.me();
        (me.view_nodes[old_idx], me.view_nodes[idx])
    };
    ctx.at(old).unwrap().set_display(Display::Hidden);
    ctx.at(new).unwrap().set_display(Display::Flex);
    ctx.me().current_view = idx;
    tracing::info!(view = %target, "switched view");
}

/// Toggle the Ctrl one-shot latch and repaint every Ctrl key across all views
/// to reflect the armed/disarmed state.
fn toggle_latch(ctx: &mut Context<'_, Keyboard>) {
    ctx.resource_mut::<LatchedState>().ctrl = !ctx.resource::<LatchedState>().ctrl;
    repaint_ctrl_keys(ctx);
    tracing::info!("Ctrl latch: {}", ctx.resource::<LatchedState>().ctrl);
}

/// Repaint all Ctrl keys to reflect the current latched state.
fn repaint_ctrl_keys(ctx: &mut Context<'_, Keyboard>) {
    let latched = ctx.resource::<LatchedState>().ctrl;
    let (bg_role, _fg_role) = roles_for(KeyKind::Modifier, latched);
    let bg = ctx.color(bg_role);
    let keys = ctx.me().ctrl_keys.clone();
    for k in keys {
        ctx.at(k).unwrap().set_paint(Paint::Quad(Quad::new(bg).radius(6.0)));
        // The label colour is handled by the on_theme handler on the next
        // theme change; the paint update here is the important visual cue.
    }
}

// ── Shell widget (layer-shell window) ─────────────────────────────────────

/// Spawns a `zwlr_layer_surface` anchored to the bottom edge of the output,
/// stretched full-width (left + right anchors), with an exclusive zone so the
/// compositor reserves space and no other client's content is hidden behind
/// the keyboard. `KeyboardInteractivity::OnDemand` lets the user tap keys
/// without the OSK stealing keyboard focus from the focused app.
struct Shell;

struct ShellBuilder {
    root: NodeId,
    font: FontId,
    keymap: Keymap,
}

impl Build for ShellBuilder {
    type Widget = Shell;
}

impl Widget for Shell {
    type Builder = ShellBuilder;
    fn build(b: ShellBuilder, _me: Handle<Self>, s: &mut Spawner<'_, Self>) -> Self {
        let role = Role::Layer(LayerRole {
            layer: Layer::Top,
            anchor: Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
            exclusive_zone: 280,
            namespace: "mechanix-keyboard".into(),
            keyboard_interactivity: KeyboardInteractivity::OnDemand,
        });

        let win = s.spawn_with(
            b.root,
            window().layout(LayoutStyle::default().column().size(auto(), px(280.0))),
            (role,),
        );

        s.spawn(win, KeyboardBuilder { font: b.font, keymap: b.keymap });
        Shell
    }
}

// ── main ──────────────────────────────────────────────────────────────────

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Load the keymap from layout.yaml (env var, local file, or bundled fallback).
    let keymap = layout::load_keymap();

    let mut app = App::new();
    app.add_module(LayoutModule)
        .add_module(PaintModule)
        .add_module(WindowModule)
        .add_module(InteractivityModule)
        .add_module(RenderModule::default())
        .insert_resource(Atlas::new());
    app.insert_resource(LatchedState::default());

    app.add_module(MechanixTheme::dark())
        .add_module(RingModule::default())
        .add_module(
            WaylandModule::new()
                .bind::<WlCompositor>()
                .bind::<ZwpLinuxDmabufV1>()
                .bind::<XdgWmBase>()
                .bind::<WlSeat>(),
        )
        .add_module(PresentationModule {
            app_id: "mecha.keyboard".into(),
            budget: Budget::default(),
        });

    // Virtual-keyboard-v1 and input-method-v2 modules — bind their globals
    // conditionally from `Globals` and handle their protocol events. These
    // must install after `WaylandModule` (provides `Globals` + `Wayland`) and
    // after the seat is bound.
    app.add_module(virtual_keyboard::VirtualKeyboardModule);
    app.add_module(input_method::InputMethodModule);

    // Load the font for key labels.
    let font = app
        .resource_mut::<Atlas>()
        .add_font(include_bytes!("../resources/RobotoMono-Regular.ttf"))
        .expect("RobotoMono loads");

    let root = app.root();
    app.spawn(root, ShellBuilder { root, font, keymap });
    app.run();
}
