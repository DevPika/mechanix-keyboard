use serde::Deserialize;
use std::collections::BTreeMap;
use std::{env, fs};
use tracing::{info, warn};

use crate::MechanixKeyboardState;

#[derive(Debug, Deserialize)]
pub struct Layout {
    pub outlines: BTreeMap<String, Outline>,
    pub views: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct Outline {
    pub width: f32,
    pub height: f32,
}

static FALLBACK_LAYOUT: &str = include_str!("../resources/layout.yaml");
pub static MARGIN: f32 = 20.0;

/// Load layout on startup.
pub fn module<S>() -> impl app::RegisteredModule<MechanixKeyboardState, S> {
    app::Module::new().on(|s: &mut MechanixKeyboardState, _: &app::Start| {
        let contents: String;
        match env::var("MECHA_KBD_LAYOUT") {
            Ok(path) => {
                contents = fs::read_to_string(path).expect("Error: Failed to read layout file")
            }
            Err(_) => {
                warn!("Warning: No config path provided! Using fallback...");
                contents = FALLBACK_LAYOUT.into();
            }
        }

        let layout: Layout = yaml_serde::from_str(&contents).expect("Error: Failed to parse yaml");

        s.layout = Some(layout);

        for (view_name, rows) in &s.layout.as_ref().unwrap().views {
            info!("view: {view_name}");
            for row in rows {
                info!("  {row}");
            }
        }
    })
}
