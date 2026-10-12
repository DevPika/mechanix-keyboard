use crate::font::Font;
use crate::key_style::{self, KeyLook, KeyStyle, KeyStyleSpec};
use crate::{color_role, shape, spacing};
use mecha_wayland::prelude::{ColorRole, ThemeMode};
use serde::Deserialize;
use std::collections::HashMap;
use std::{env, fs};
use tracing::{info, warn};
use xkbcommon::xkb::{self, Keysym};

/// A simple 2D rectangle (replaces the old `utils::Rect` which is no longer
/// a direct dependency — the new mecha-wayland layout engine uses `Val`/`px`).
#[derive(Debug, Clone, Copy, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
}

#[derive(Debug, Deserialize)]
struct Layout {
    #[serde(default)]
    keyboard: KeyboardSpec,
    /// Named key colour styles; a button picks one with `key_style:`.
    #[serde(default)]
    key_styles: HashMap<String, KeyStyleSpec>,
    outlines: HashMap<String, Outline>,
    views: HashMap<String, Vec<String>>,
    #[serde(default)]
    buttons: HashMap<String, Button>,
}

/// Applies to the entire keyboard container layout
#[derive(Debug, Default, Deserialize)]
struct KeyboardSpec {
    mode: Option<String>,
    background: Option<String>,
    radius: Option<String>,
    gap: Option<f32>,
    #[serde(rename = "row-gap")]
    row_gap: Option<f32>,
    padding: Option<f32>,
    width: Option<f32>,
    height: Option<f32>,
    font: Option<String>,
    #[serde(rename = "font-size")]
    font_size: Option<u16>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct Outline {
    width: f32,
    height: f32,
}

/// A squeekboard button's `action:` value — either a bare name (`erase`,
/// `show_prefs`) or a structured single-key map (`set_view`, `locking`). Tried
/// as untagged variants in order; `Other` is the `IgnoredAny` catch-all, so an
/// unrecognised structured action still parses (and later resolves to
/// `Unhandled`) instead of failing the whole layout.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ActionSpec {
    Named(String),
    SetView { set_view: String },
    Locking { locking: LockingSpec },
    Other(serde::de::IgnoredAny),
}

/// The body of a squeekboard `locking` action: the view tapped *to* (`lock_view`),
/// and the view tapped *back to* (`unlock_view`) once the lock view is current.
#[derive(Debug, Deserialize)]
struct LockingSpec {
    lock_view: String,
    unlock_view: String,
}

#[derive(Debug, Default, Deserialize)]
struct Button {
    outline: Option<String>,
    label: Option<String>,
    icon: Option<String>,
    /// Emit fields the Key action resolves from; see `resolve_action`.
    keysym: Option<String>,
    text: Option<String>,
    #[serde(default)]
    action: Option<ActionSpec>,
    modifier: Option<String>,
    key_style: Option<String>,
}

/// Size used when a button names an outline that isn't defined (and no
/// `default` outline exists to fall back to). Keeps conversion total.
const FALLBACK_OUTLINE: Outline = Outline {
    width: 50.0,
    height: 50.0,
};

#[derive(Debug)]
pub struct Keymap {
    pub views: Vec<View>,
    pub keyboard: KeyboardStyle,
}

#[derive(Debug, Clone, Copy)]
pub struct KeyboardStyle {
    pub mode: ThemeMode,
    pub background_color: ColorRole,
    /// Key corner radius in logical px, resolved from a `Shape` token.
    pub radius: f32,
    /// Spacing in logical px, each resolved from a `Spacing` token.
    pub gap: f32,
    pub row_gap: f32,
    pub padding: f32,
    /// Window width in logical px; `None` spans the whole output.
    pub width: Option<f32>,
    /// Window height in logical px.
    pub height: f32,
    pub font: Font,
    pub font_size: u16,
}

impl Default for KeyboardStyle {
    fn default() -> Self {
        Self {
            mode: ThemeMode::Dark,
            background_color: ColorRole::Surface,
            radius: 6.0,
            gap: 4.0,
            row_gap: 4.0,
            padding: 4.0,
            width: None,
            height: 280.0,
            font: Font::Geist,
            font_size: 16,
        }
    }
}

// TODO: Have a generic function to check for the positive values
/// Keep a window dimension only if it's a positive size, warning otherwise.
fn positive(field: &str, value: Option<f32>) -> Option<f32> {
    value.filter(|&px| {
        let ok = px > 0.0;
        if !ok {
            warn!("keyboard {field} {px} must be greater than 0; using the default");
        }
        ok
    })
}

/// Resolve an optional `Spacing` token to px, warning and keeping `default`
/// for a value that isn't a token.
fn resolve_spacing(field: &str, value: Option<f32>, default: f32) -> f32 {
    match value {
        None => default,
        Some(px) => match spacing::from_px(px) {
            Some(token) => token.dp(),
            None => {
                warn!("keyboard {field} {px} is not a spacing token; using {default}");
                default
            }
        },
    }
}

impl KeyboardStyle {
    /// Resolve the raw `keyboard:` block; an unrecognised value warns and
    /// keeps its default.
    fn resolve(spec: &KeyboardSpec) -> Self {
        // TODO: Have it behind a trait
        let default = Self::default();
        let mode = match spec.mode.as_deref() {
            None => default.mode,
            Some("dark") => ThemeMode::Dark,
            Some("light") => ThemeMode::Light,
            Some(other) => {
                warn!(
                    "keyboard mode {other:?} is unknown (expected `dark` or `light`); using dark"
                );
                default.mode
            }
        };
        let background_color = match spec.background.as_deref() {
            None => default.background_color,
            Some(name) => color_role::from_name(name).unwrap_or_else(|| {
                warn!("keyboard background {name:?} is not a colour role; using `surface`");
                default.background_color
            }),
        };
        let radius = match spec.radius.as_deref() {
            None => default.radius,
            Some(name) => match shape::from_name(name) {
                // No component height is needed: `full` isn't accepted.
                Some(shape) => shape.resolve_radius_dp(0.0),
                None => {
                    warn!("keyboard radius {name:?} is not a shape token; using the default");
                    default.radius
                }
            },
        };
        let font = match spec.font.as_deref() {
            None => default.font,
            Some(name) => Font::from_name(name).unwrap_or_else(|| {
                warn!("keyboard font {name:?} is not a bundled font; using the default");
                default.font
            }),
        };
        let font_size = match spec.font_size {
            None => default.font_size,
            Some(0) => {
                warn!("keyboard font-size must be greater than 0; using the default");
                default.font_size
            }
            Some(px) => px,
        };
        Self {
            mode,
            background_color,
            radius,
            gap: resolve_spacing("gap", spec.gap, default.gap),
            row_gap: resolve_spacing("row-gap", spec.row_gap, default.row_gap),
            padding: resolve_spacing("padding", spec.padding, default.padding),
            width: positive("width", spec.width),
            height: positive("height", spec.height).unwrap_or(default.height),
            font,
            font_size,
        }
    }
}

/// One selectable arrangement of keys (e.g. `base`, `upper`).
#[derive(Debug)]
pub struct View {
    pub name: String,
    pub rows: Vec<Row>,
}

/// One horizontal line of keys within a view.
#[derive(Debug)]
pub struct Row {
    pub keys: Vec<Key>,
}

/// What a key draws in its cell: a text label *or* a symbolic icon, never both.
/// The icon is its name (an SVG's file stem under `resources/icons`); the `Key`
/// widget looks up the rasterized sprite in the `Icons` resource.
#[derive(Debug, Clone)]
pub enum KeyFace {
    Text(String),
    Icon(String),
}

/// What a Key *does* when activated — the behavioural counterpart to `KeyFace`.
/// Resolved at IR-build time from a squeekboard button's `keysym`/`text`/`action`
/// fields.
#[derive(Debug, Clone)]
pub enum KeyAction {
    /// Emit a single resolved xkb keysym (letters, digits, Return, BackSpace…).
    EmitKeysym(Keysym),
    /// Emit literal text, one keysym per char (e.g. the space key's `" "`).
    EmitText(String),
    /// Switch to a named view and stay there (squeekboard `set_view`). Sticky —
    /// no auto-return.
    SetView(String),
    /// Flip between two named views by current view (squeekboard `locking`): go
    /// to `lock` normally, or back to `unlock` when `lock` is already current.
    ToggleView { lock: String, unlock: String },
    /// Arm a real modifier (Control) for exactly the *next* keystroke, then it
    /// auto-clears — the OSK one-shot latch. The modifier's mask is OR'd into the
    /// next emitted Keystroke; tapping again disarms. Carries the squeekboard
    /// modifier name (e.g. `Control`).
    LatchModifier(String),
    /// A squeekboard action not wired this pass (prefs, and modifiers other than
    /// Control). The key still draws and hit-tests; tapping it logs but types
    /// nothing. Carries a name for that log.
    Unhandled(String),
}

/// One drawable key: what to draw, what it does, and where (logical units).
#[derive(Debug, Clone)]
pub struct Key {
    pub face: KeyFace,
    pub action: KeyAction,
    pub rect: Rect,
    pub touch_area: Rect,
    /// Colours per `KeyState`, from the button's `key_style:` (or `normal`)
    /// and the `latched` style.
    pub look: KeyLook,
}

impl Key {
    /// A human-readable label for bring-up tracing (hover/tap). Icon keys log
    /// their icon name.
    pub fn display_label(&self) -> &str {
        match &self.face {
            KeyFace::Text(s) => s,
            KeyFace::Icon(name) => name,
        }
    }

    /// The text to render on the key: either the label or the icon name.
    pub fn label_text(&self) -> &str {
        self.display_label()
    }
}

impl Row {
    /// The row's keys.
    pub fn keys(&self) -> &[Key] {
        &self.keys
    }
}

impl View {
    /// Every key in the view, flattened across its rows.
    pub fn keys(&self) -> impl Iterator<Item = &Key> {
        self.rows.iter().flat_map(|row| row.keys.iter())
    }

    /// The view's intrinsic logical width: the right edge of its widest row.
    pub fn width(&self) -> f32 {
        self.keys().map(|k| k.rect.right()).fold(0.0_f32, f32::max)
    }

    /// The view's intrinsic logical height: the bottom edge of its last row.
    pub fn height(&self) -> f32 {
        self.keys().map(|k| k.rect.bottom()).fold(0.0_f32, f32::max)
    }
}

impl Keymap {
    /// Every key in the keymap, flattened across every view.
    pub fn keys(&self) -> impl Iterator<Item = &Key> {
        self.views.iter().flat_map(|view| view.keys())
    }

    /// Look a view up by name (e.g. the initial `base` view to render).
    pub fn view(&self, name: &str) -> Option<&View> {
        self.views.iter().find(|v| v.name == name)
    }

    /// The index of the view with this name, for storing as the current view.
    /// Resolved once per switch so per-frame reads stay O(1) array indexing.
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.views.iter().position(|v| v.name == name)
    }

    /// Convert a parsed squeekboard layout into the IR, resolving each key's
    /// label and geometry.
    fn from_layout(layout: &Layout) -> Self {
        let styles = key_style::resolve_all(&layout.key_styles);
        let views = layout
            .views
            .iter()
            .map(|(name, rows)| View::resolve(layout, &styles, name, rows))
            .collect();
        let keyboard = KeyboardStyle::resolve(&layout.keyboard);
        Keymap { views, keyboard }
    }
}

impl View {
    /// Resolve one view's rows into laid-out keys.
    ///
    /// Two passes: first resolve every key's face + outline, then flow them.
    /// Keys butt together left-to-right with no gap; each row's height is the
    /// max key height in it; rows stack top-down; and each row is centred
    /// within the view's width (the widest row), matching squeekboard's look.
    fn resolve(
        layout: &Layout,
        styles: &HashMap<String, KeyStyle>,
        name: &str,
        rows: &[String],
    ) -> View {
        // Pass 1: resolve face + action + size + look for every key, grouped by row.
        let sized: Vec<Vec<(KeyFace, KeyAction, Outline, KeyLook)>> = rows
            .iter()
            .map(|row| {
                row.split_whitespace()
                    .map(|token| {
                        let button = layout.buttons.get(token);
                        let outline = resolve_outline(layout, button);
                        let face = resolve_face(token, button);
                        let action = resolve_action(token, button);
                        let look = KeyLook::for_button(
                            styles,
                            token,
                            button.and_then(|b| b.key_style.as_deref()),
                            types_digit(&action),
                        );
                        (face, action, outline, look)
                    })
                    .collect()
            })
            .collect();

        // The view is as wide as its widest row.
        let view_width = sized
            .iter()
            .map(|row| row.iter().map(|(_, _, o, _)| o.width).sum::<f32>())
            .fold(0.0_f32, f32::max);

        // Pass 2: flow each row, centred, stacking downward.
        let mut y = 0.0_f32;
        let mut out_rows = Vec::with_capacity(sized.len());
        for row in &sized {
            let row_width: f32 = row.iter().map(|(_, _, o, _)| o.width).sum();
            let row_height = row
                .iter()
                .map(|(_, _, o, _)| o.height)
                .fold(0.0_f32, f32::max);
            let mut x = (view_width - row_width) / 2.0;
            let keys = row
                .iter()
                .map(|(face, action, o, look)| {
                    let rect = Rect::new(x, y, o.width, o.height);
                    let key = Key {
                        face: face.clone(),
                        action: action.clone(),
                        rect,
                        touch_area: rect,
                        look: *look,
                    };
                    x += o.width;
                    key
                })
                .collect();
            out_rows.push(Row { keys });
            y += row_height;
        }

        View {
            name: name.to_string(),
            rows: out_rows,
        }
    }
}

/// Resolve a button token's face. Icon wins over label when both are set
/// (matching squeekboard) — and warns. With neither icon nor label, the
/// button's own name is the label.
fn resolve_face(token: &str, button: Option<&Button>) -> KeyFace {
    if let Some(icon) = button.and_then(|b| b.icon.as_deref()) {
        if button.and_then(|b| b.label.as_deref()).is_some() {
            warn!("button {token:?} sets both `icon` and `label`; using icon {icon:?}");
        }
        return KeyFace::Icon(icon.to_string());
    }
    let label = button
        .and_then(|b| b.label.clone())
        .unwrap_or_else(|| token.to_string());
    KeyFace::Text(label)
}

fn types_digit(action: &KeyAction) -> bool {
    let text = match action {
        KeyAction::EmitKeysym(ks) => char::from_u32(xkb::keysym_to_utf32(*ks)),
        KeyAction::EmitText(text) => {
            let mut chars = text.chars();
            chars.next().filter(|_| chars.next().is_none())
        }
        KeyAction::SetView(_)
        | KeyAction::ToggleView { .. }
        | KeyAction::LatchModifier(_)
        | KeyAction::Unhandled(_) => None,
    };
    text.is_some_and(|c| c.is_ascii_digit())
}

/// Resolve a button token's Key action — what it emits when tapped. Priority:
/// explicit `keysym:` → `action: erase` (→ BackSpace) → `text:` → a single-char
/// token/label (→ its keysym). A `modifier`, a structured/other `action`, or a
/// multi-char label with no keysym all resolve to `Unhandled` this pass.
fn resolve_action(token: &str, button: Option<&Button>) -> KeyAction {
    if let Some(b) = button {
        if let Some(name) = b.keysym.as_deref() {
            let ks = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS);
            if ks.raw() != 0 {
                return KeyAction::EmitKeysym(ks);
            }
            warn!("keysym {name:?} on button {token:?} is unknown; key won't type");
            return KeyAction::Unhandled(token.to_string());
        }
        match &b.action {
            Some(ActionSpec::Named(a)) if a == "erase" => {
                return KeyAction::EmitKeysym(xkb::keysym_from_name(
                    "BackSpace",
                    xkb::KEYSYM_NO_FLAGS,
                ));
            }
            Some(ActionSpec::Named(a)) => return KeyAction::Unhandled(a.clone()),
            Some(ActionSpec::SetView { set_view }) => {
                return KeyAction::SetView(set_view.clone());
            }
            Some(ActionSpec::Locking { locking }) => {
                return KeyAction::ToggleView {
                    lock: locking.lock_view.clone(),
                    unlock: locking.unlock_view.clone(),
                };
            }
            Some(ActionSpec::Other(_)) => return KeyAction::Unhandled(token.to_string()),
            None => {}
        }
        // A modifier button latches for the next keystroke. Only Control is wired
        // this pass (see `virtual_keyboard::toggle_latch` and its `mod_masks`);
        // every other modifier still defers to `Unhandled`.
        if let Some(m) = b.modifier.as_deref() {
            if m == "Control" {
                return KeyAction::LatchModifier(m.to_string());
            }
            return KeyAction::Unhandled(token.to_string());
        }
        if let Some(text) = &b.text {
            return KeyAction::EmitText(text.clone());
        }
        if let Some(ks) = b.label.as_deref().and_then(single_char_keysym) {
            return KeyAction::EmitKeysym(ks);
        }
    }
    // No button entry (or nothing above matched): a single-char token is itself
    // the keysym — this covers every plain letter/digit/punctuation key.
    if let Some(ks) = single_char_keysym(token) {
        return KeyAction::EmitKeysym(ks);
    }
    KeyAction::Unhandled(token.to_string())
}

/// The keysym for a single-character string, or `None` if `s` isn't exactly one
/// char or that char has no keysym.
fn single_char_keysym(s: &str) -> Option<Keysym> {
    let mut chars = s.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return None;
    };
    let ks = Keysym::from_char(c);
    (ks.raw() != 0).then_some(ks)
}

/// Resolve a button's outline size: the button's named outline, else the
/// `default` outline, else a hard fallback. Warns on a dangling outline name.
fn resolve_outline(layout: &Layout, button: Option<&Button>) -> Outline {
    let name = button
        .and_then(|b| b.outline.as_deref())
        .unwrap_or("default");
    if let Some(outline) = layout.outlines.get(name) {
        return *outline;
    }
    warn!("outline {name:?} not defined; falling back to `default`");
    layout
        .outlines
        .get("default")
        .copied()
        .unwrap_or(FALLBACK_OUTLINE)
}

static FALLBACK_LAYOUT: &str = include_str!("../resources/layout.yaml");

/// Load and convert the layout on startup. Checks `MECHA_KBD_LAYOUT` env var,
/// then `layout.yaml` in the working directory, then the bundled fallback.
pub fn load_keymap() -> Keymap {
    let contents = match env::var("MECHA_KBD_LAYOUT") {
        Ok(path) => fs::read_to_string(path).expect("Error: Failed to read layout file"),
        Err(_) => {
            let local = std::path::Path::new("layout.yaml");
            if local.is_file() {
                info!("Using layout.yaml from working directory");
                fs::read_to_string(local).expect("Error: Failed to read layout file")
            } else {
                warn!("Warning: No config path provided! Using fallback...");
                FALLBACK_LAYOUT.into()
            }
        }
    };

    let layout: Layout = yaml_serde::from_str(&contents).expect("Error: Failed to parse yaml");
    let keymap = Keymap::from_layout(&layout);

    info!("loaded keymap: {} view(s)", keymap.views.len());
    for view in &keymap.views {
        info!("  view {}: {} key(s)", view.name, view.keys().count());
    }

    keymap
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Button` with only the fields a test cares about set.
    fn button(f: impl FnOnce(&mut Button)) -> Button {
        let mut b = Button::default();
        f(&mut b);
        b
    }

    #[test]
    fn plain_char_token_resolves_to_its_keysym() {
        for tok in ["q", "1", ",", "."] {
            let c = tok.chars().next().unwrap();
            assert!(
                matches!(resolve_action(tok, None), KeyAction::EmitKeysym(ks) if ks == Keysym::from_char(c)),
                "token {tok:?} should emit its own keysym",
            );
        }
    }

    #[test]
    fn explicit_keysym_field_wins() {
        let b = button(|b| b.keysym = Some("Return".into()));
        let want = xkb::keysym_from_name("Return", xkb::KEYSYM_NO_FLAGS);
        assert!(
            matches!(resolve_action("Return", Some(&b)), KeyAction::EmitKeysym(ks) if ks == want)
        );
    }

    #[test]
    fn erase_action_maps_to_backspace() {
        let b = button(|b| b.action = Some(ActionSpec::Named("erase".into())));
        let want = xkb::keysym_from_name("BackSpace", xkb::KEYSYM_NO_FLAGS);
        assert!(
            matches!(resolve_action("BackSpace", Some(&b)), KeyAction::EmitKeysym(ks) if ks == want)
        );
    }

    #[test]
    fn text_field_resolves_to_emit_text() {
        let b = button(|b| b.text = Some(" ".into()));
        assert!(matches!(resolve_action("space", Some(&b)), KeyAction::EmitText(t) if t == " "));
    }

    #[test]
    fn set_view_action_resolves_to_setview() {
        let b = button(|b| {
            b.action = Some(ActionSpec::SetView {
                set_view: "symbols".into(),
            })
        });
        assert!(matches!(
            resolve_action("show_symbols", Some(&b)),
            KeyAction::SetView(v) if v == "symbols"
        ));
    }

    #[test]
    fn locking_action_resolves_to_toggleview() {
        let b = button(|b| {
            b.action = Some(ActionSpec::Locking {
                locking: LockingSpec {
                    lock_view: "upper".into(),
                    unlock_view: "base".into(),
                },
            })
        });
        assert!(matches!(
            resolve_action("Shift_L", Some(&b)),
            KeyAction::ToggleView { lock, unlock } if lock == "upper" && unlock == "base"
        ));
    }

    #[test]
    fn deferred_actions_are_unhandled() {
        // An unrecognised structured action, a bare non-erase action, and an
        // unwired modifier all defer this pass — none should type or switch.
        let unknown = button(|b| b.action = Some(ActionSpec::Other(serde::de::IgnoredAny)));
        assert!(matches!(
            resolve_action("mystery", Some(&unknown)),
            KeyAction::Unhandled(_)
        ));

        let prefs = button(|b| b.action = Some(ActionSpec::Named("show_prefs".into())));
        assert!(matches!(
            resolve_action("preferences", Some(&prefs)),
            KeyAction::Unhandled(_)
        ));

        // Alt is a modifier, but not wired to latch this pass — still deferred.
        let alt = button(|b| b.modifier = Some("Alt".into()));
        assert!(matches!(
            resolve_action("Alt", Some(&alt)),
            KeyAction::Unhandled(_)
        ));
    }

    #[test]
    fn control_modifier_latches() {
        let ctrl = button(|b| b.modifier = Some("Control".into()));
        assert!(matches!(
            resolve_action("Ctrl", Some(&ctrl)),
            KeyAction::LatchModifier(m) if m == "Control"
        ));
    }

    #[test]
    fn fallback_structured_actions_resolve() {
        // End-to-end: the untagged `ActionSpec` must parse real layout YAML, and
        // the structured actions must reach their view-switch variants.
        let layout: Layout = yaml_serde::from_str(FALLBACK_LAYOUT).expect("fallback parses");
        let keymap = Keymap::from_layout(&layout);

        let has_set_view = keymap
            .keys()
            .any(|k| matches!(&k.action, KeyAction::SetView(v) if v == "symbols"));
        assert!(has_set_view, "a `set_view: symbols` key should be SetView");

        let has_toggle = keymap.keys().any(|k| {
            matches!(&k.action, KeyAction::ToggleView { lock, unlock } if lock == "upper" && unlock == "base")
        });
        assert!(has_toggle, "Shift_L should be ToggleView upper/base");
    }

    #[test]
    fn fallback_layout_base_view_has_expected_actions() {
        let layout: Layout = yaml_serde::from_str(FALLBACK_LAYOUT).expect("fallback parses");
        let keymap = Keymap::from_layout(&layout);
        let base = keymap.view("base").expect("base view exists");

        // Every base-view key resolves to *some* action, and the ones that type
        // are keysym/text — no base key is left ambiguous.
        let typeable = base
            .keys()
            .filter(|k| matches!(k.action, KeyAction::EmitKeysym(_) | KeyAction::EmitText(_)))
            .count();
        // 26 letters plus `, . space ⌫ DONE` in the Figma 4-row layout.
        assert!(
            typeable >= 31,
            "base view should have many typeable keys, got {typeable}"
        );
    }
}
