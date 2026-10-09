//! Named key colour styles from the `key_styles:` block of `layout.yaml`.
//! Each style pairs a background and a foreground (label) colour role, and a
//! button picks one with `key_style:`. A key without one uses `normal`, and an
//! armed modifier uses `latched`.

use crate::color_role;
use mecha_wayland::prelude::ColorRole;
use serde::Deserialize;
use std::collections::HashMap;
use tracing::warn;

pub const NORMAL: &str = "normal";
pub const LATCHED: &str = "latched";

#[derive(Debug, Default, Deserialize)]
pub struct KeyStyleSpec {
    background: Option<String>,
    foreground: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct KeyStyle {
    pub background: Option<ColorRole>,
    pub foreground: Option<ColorRole>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    Normal,
    Latched,
}

impl KeyState {
    pub fn latched_if(armed: bool) -> Self {
        if armed { Self::Latched } else { Self::Normal }
    }
}

/// Every style a key can be drawn with, one per `KeyState`.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct KeyLook {
    normal: KeyStyle,
    latched: KeyStyle,
}

impl KeyLook {
    /// The look of a button: its `key_style` (or `normal`) normally, and the
    /// shared `latched` style while armed.
    pub fn for_button(styles: &HashMap<String, KeyStyle>, token: &str, name: Option<&str>) -> Self {
        Self {
            normal: for_button(styles, token, name),
            latched: styles.get(LATCHED).copied().unwrap_or_default(),
        }
    }

    pub fn style(&self, state: KeyState) -> KeyStyle {
        match state {
            KeyState::Normal => self.normal,
            KeyState::Latched => self.latched,
        }
    }
}

/// Resolve every style in `key_styles:`, warning on role names that aren't
/// colour roles (that half of the style is then left unset).
pub fn resolve_all(specs: &HashMap<String, KeyStyleSpec>) -> HashMap<String, KeyStyle> {
    specs
        .iter()
        .map(|(name, spec)| {
            let style = KeyStyle {
                background: role(name, "background", spec.background.as_deref()),
                foreground: role(name, "foreground", spec.foreground.as_deref()),
            };
            (name.clone(), style)
        })
        .collect()
}

/// The style a button asked for, falling back to `normal` (with a warning if
/// the name it asked for isn't defined). Unstyled if `normal` isn't defined.
fn for_button(styles: &HashMap<String, KeyStyle>, token: &str, name: Option<&str>) -> KeyStyle {
    if let Some(name) = name {
        if let Some(style) = styles.get(name) {
            return *style;
        }
        warn!("button {token:?} uses key_style {name:?}, which isn't defined; using `{NORMAL}`");
    }
    styles.get(NORMAL).copied().unwrap_or_default()
}

fn role(style: &str, field: &str, name: Option<&str>) -> Option<ColorRole> {
    let name = name?;
    let role = color_role::from_name(name);
    if role.is_none() {
        warn!("key_style {style:?} {field} {name:?} is not a colour role; ignoring it");
    }
    role
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styles() -> HashMap<String, KeyStyle> {
        let specs: HashMap<String, KeyStyleSpec> = yaml_serde::from_str(
            "normal:   { background: surface-container-high, foreground: on-surface }\n\
             function: { background: surface-container-highest }\n\
             broken:   { background: not-a-role }\n",
        )
        .unwrap();
        resolve_all(&specs)
    }

    #[test]
    fn button_gets_its_named_style() {
        let style = for_button(&styles(), "Shift_L", Some("function"));
        assert_eq!(style.background, Some(ColorRole::SurfaceContainerHighest));
        assert_eq!(style.foreground, None);
    }

    #[test]
    fn unstyled_or_unknown_style_falls_back_to_normal() {
        let normal = styles()[NORMAL];
        assert_eq!(for_button(&styles(), "q", None), normal);
        assert_eq!(for_button(&styles(), "q", Some("nope")), normal);
    }

    #[test]
    fn look_picks_style_by_state() {
        let mut styles = styles();
        styles.insert(
            LATCHED.into(),
            KeyStyle {
                background: Some(ColorRole::Primary),
                foreground: None,
            },
        );
        let look = KeyLook::for_button(&styles, "Ctrl", Some("function"));
        assert_eq!(look.style(KeyState::Normal), styles["function"]);
        assert_eq!(look.style(KeyState::Latched), styles[LATCHED]);
    }

    #[test]
    fn bad_role_name_leaves_that_colour_unset() {
        assert_eq!(styles()["broken"], KeyStyle::default());
    }
}
