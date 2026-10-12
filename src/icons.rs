use mecha_wayland::prelude::*;
use std::collections::HashMap;
use std::fs;

const ICONS_DIR: &str = "./resources/icons";

#[derive(Debug)]
pub struct Icons {
    icons: HashMap<String, SpriteId>,
}

impl Resource for Icons {}

impl Icons {
    pub fn load(atlas: &mut Atlas, size: u32) -> Self {
        let mut icons: HashMap<String, SpriteId> = HashMap::new();
        for entry in fs::read_dir(ICONS_DIR).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("svg") {
                continue;
            }
            let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
            let bytes = fs::read(&path).unwrap();
            let bitmap = Bitmap::from_svg(&bytes, size).unwrap();
            let sprite = atlas.insert(Class::Icon, &bitmap).unwrap();
            icons.insert(name, sprite);
        }
        Self { icons }
    }

    pub fn get(&self, name: &str) -> Option<SpriteId> {
        self.icons.get(name).copied()
    }
}
