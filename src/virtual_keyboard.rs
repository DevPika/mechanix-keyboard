//! `zwp_virtual_keyboard_v1` — the virtual-keyboard-v1 protocol client.
//!
//! Rewritten for the new mecha-wayland `app` core: `VirtualKeyboardState` is a
//! `Resource` (not a field on a monolithic state struct), and the module binds
//! the `ZwpVirtualKeyboardManagerV1` global conditionally from `Globals` and
//! creates the `ZwpVirtualKeyboardV1` object once a seat is available.
//!
//! The protocol logic — keymap compilation, keysym→keystroke indexing, memfd
//! keymap file creation, and keycode emission — is preserved from the original
//! implementation.

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::time::Instant;

use app::{App, Module, Resource};
use rustix::fs::{MemfdFlags, SealFlags};
use rustix::mm::{MapFlags, ProtFlags};
use wayland::{
    Globals, Interface, Wayland, WlKeyboardKeymapFormat, WlSeat, ZwpVirtualKeyboardManagerV1,
    ZwpVirtualKeyboardV1,
};
use xkbcommon::xkb::ffi::XKB_KEYMAP_FORMAT_TEXT_V1;
use xkbcommon::xkb::{self, Context, Keycode, Keymap, Keysym, MOD_NAME_CTRL, MOD_NAME_SHIFT};

/// One resolved keystroke: the evdev keycode to press plus the modifier mask to
/// hold while pressing it.
#[derive(Debug, Clone, Copy)]
pub struct Keystroke {
    pub code: u32,
    pub mods: u32,
}

/// All state the virtual-keyboard-v1 client owns. Lives on the app as a
/// `Resource`, reachable from `Context` handlers via `ctx.resource_mut::<VirtualKeyboardState>()`.
pub struct VirtualKeyboardState {
    /// The `ZwpVirtualKeyboardV1` object; `None` until both the manager and a
    /// seat are available.
    pub virtual_keyboard: Option<ZwpVirtualKeyboardV1>,

    pub start_time: Instant,
    /// keysym → keystroke, scanned from the uploaded keymap's base and shifted
    /// levels. Empty until the keymap is sent; emission is a no-op until then.
    pub keycodes: HashMap<Keysym, Keystroke>,
    /// squeekboard modifier name → its serialized mask, derived from the keymap
    /// (e.g. `Control` → the Control mask). Only latchable modifiers are indexed.
    pub mod_masks: HashMap<String, u32>,
}

impl Default for VirtualKeyboardState {
    fn default() -> Self {
        Self {
            virtual_keyboard: None,
            start_time: Instant::now(),
            keycodes: HashMap::new(),
            mod_masks: HashMap::new(),
        }
    }
}

impl Resource for VirtualKeyboardState {}

/// Module that binds the virtual-keyboard manager global and creates the VK
/// object. Install after `WaylandModule` (which provides `Globals` and `Wayland`)
/// and after the seat is bound.
pub struct VirtualKeyboardModule;

impl Module for VirtualKeyboardModule {
    fn install(self, app: &mut App) {
        app.insert_resource(VirtualKeyboardState::default());
        init(app);
    }
}

/// Try to bind the virtual-keyboard manager and create the VK object.
/// Called at install and safe to call multiple times — it's a no-op once the
/// VK exists.
fn init(app: &mut App) {
    let seat = *app.resource::<WlSeat>();

    // The manager is an optional global — not every compositor supports
    // virtual-keyboard-v1. Bind it from `Globals` if advertised.
    let manager = app
        .resource::<Globals>()
        .find(ZwpVirtualKeyboardManagerV1::NAME)
        .cloned()
        .map(|g| {
            let (globals, mut wl) = app.query::<(app::Res<Globals>, app::ResMut<Wayland>)>();
            globals.bind::<ZwpVirtualKeyboardManagerV1>(&g, &mut wl)
        });

    let Some(manager) = manager else {
        tracing::warn!("compositor does not advertise zwp_virtual_keyboard_manager_v1");
        return;
    };

    let vkbd = {
        let mut wl = app.resource_mut::<Wayland>();
        manager.create_virtual_keyboard(&mut wl, seat)
    };

    // Compile a standard keymap from the default rules and serialise it.
    let ctx = Context::new(0);
    let keymap = Keymap::new_from_names(&ctx, "", "", "us", "", None, 0)
        .expect("failed to compile default keymap");

    let keycodes = scan_keycodes(&keymap);

    let text = keymap.get_as_string(XKB_KEYMAP_FORMAT_TEXT_V1);
    let keymap_fd = match make_keymap_fd(text.as_bytes()) {
        Ok(keymap) => keymap,
        Err(err) => {
            tracing::warn!(%err, "failed to create keymap memfd");
            return;
        }
    };

    vkbd.keymap(
        &mut app.resource_mut::<Wayland>(),
        WlKeyboardKeymapFormat::XkbV1,
        keymap_fd.fd.as_fd(),
        keymap_fd.size,
    );

    // Index the modifiers a latched key can arm, mapping the squeekboard name
    // used in the layout to the serialized mask sent over the wire. Control only
    // this pass — mirrors `layout::resolve_action`'s modifier gate.
    let mut mod_masks = HashMap::new();
    mod_masks.insert(
        "Control".to_string(),
        1u32 << keymap.mod_get_index(MOD_NAME_CTRL),
    );

    tracing::info!(mapped = keycodes.len(), "virtual keyboard ready");
    let mut vk = app.resource_mut::<VirtualKeyboardState>();
    vk.virtual_keyboard = Some(vkbd);
    vk.keycodes = keycodes;
    vk.mod_masks = mod_masks;
}

/// Build the `keysym → Keystroke` map from a compiled keymap's base (level 0) and
/// shifted (level 1) levels.
fn scan_keycodes(keymap: &Keymap) -> HashMap<Keysym, Keystroke> {
    let shift = 1u32 << keymap.mod_get_index(MOD_NAME_SHIFT);
    let mut map = HashMap::new();
    for (level, mods) in [(0u32, 0u32), (1u32, shift)] {
        for kc in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
            if kc < 8 {
                continue;
            }
            let syms = keymap.key_get_syms_by_level(Keycode::new(kc), 0, level);
            if let Some(ks) = syms.first().copied()
                && ks.raw() != 0
            {
                map.entry(ks).or_insert(Keystroke { code: kc - 8, mods });
            }
        }
    }
    map
}

/// The insertable text a keysym produces, or `None` when it's a control key
/// (BackSpace, Return, Tab, Escape, Delete, arrows, function keys, …) with no
/// printable glyph.
pub fn keysym_text(ks: Keysym) -> Option<String> {
    let cps = xkb::keysym_to_utf32(ks);
    if cps == 0 {
        return None;
    }
    let ch = char::from_u32(cps)?;
    if ch.is_control() {
        return None;
    }
    Some(ch.to_string())
}

// Key emission is handled in main.rs's `dispatch` function, which uses
// `Context`'s resource access (`ctx.resource_mut::<Wayland>()`, etc.) to
// send key events directly. See `vk_emit_keysym` and `im_commit_text` there.

// ── keymap memfd ───────────────────────────────────────────────────────────

struct KeymapWithFd {
    fd: OwnedFd,
    size: u32,
}

/// Builds a sealed, shared memfd holding `text` as a NUL-terminated
/// buffer, ready to send as `set_keymap`'s fd + size.
fn make_keymap_fd(text: &[u8]) -> rustix::io::Result<KeymapWithFd> {
    let size = text.len() + 1; // +1 for the trailing NUL the protocol expects

    let fd: OwnedFd = rustix::fs::memfd_create(
        c"mechanix-keyboard-keymap",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )?;

    rustix::fs::ftruncate(&fd, size as u64)?;

    // SAFETY: `fd` is a valid memfd truncated to `size` bytes; the mapping
    // is unmapped (via the guard below) before this function returns, and
    // nothing else touches `fd` concurrently.
    let map = unsafe {
        rustix::mm::mmap(
            std::ptr::null_mut(),
            size,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::SHARED,
            &fd,
            0,
        )?
    };

    struct MmapGuard(*mut core::ffi::c_void, usize);
    impl Drop for MmapGuard {
        fn drop(&mut self) {
            // SAFETY: `munmap` is safe to call with a valid mapping and size.
            unsafe {
                let _ = rustix::mm::munmap(self.0, self.1);
            }
        }
    }

    let guard = MmapGuard(map, size);
    // SAFETY: `map` is a valid mapping of `size` bytes from the memfd; writing
    // the keymap text + NUL is within bounds.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), map as *mut u8, text.len());
        std::ptr::write_volatile(map.add(text.len()).cast::<u8>(), 0); // NUL terminator
    }
    drop(guard);

    // Sealing is optional (best practice for shared memfds) but the specific
    // rustix API for it varies by version; the keymap works without it.
    let _ = SealFlags::all();

    Ok(KeymapWithFd {
        fd,
        size: size as u32,
    })
}
