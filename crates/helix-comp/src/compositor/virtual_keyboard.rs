//! `zwp_virtual_keyboard_v1` (the shell's input method only): the keys
//! and modifiers it sends go straight to the focused client under the
//! device's own keymap, as before Smithay made virtual keyboards input
//! devices. They have been through Helix's shortcuts already (the input
//! method grabbed them), so they skip them, and the seat's own state
//! (pressed keys, modifiers, keymap) is left alone.

use std::os::unix::fs::FileExt;

use smithay::backend::input::{Event, InputEvent, KeyboardKeyEvent};
use smithay::input::keyboard::{xkb, KeyboardTarget, KeymapFile, ModifiersState};
use smithay::utils::SERIAL_COUNTER;
use smithay::wayland::virtual_keyboard::{
    VirtualKeyboardBackend, VirtualKeyboardDevice, VirtualKeyboardHandler, VirtualKeyboardSpecialEvent,
};

use super::Helix;

/// A virtual keyboard's keymap, and the modifiers it set under it.
pub struct Device {
    keymap: KeymapFile,
    xkb: Option<xkb::State>,
    mods: ModifiersState,
}

impl Device {
    fn new(keymap: KeymapFile) -> Self {
        let xkb = compile(&keymap).map(|k| xkb::State::new(&k));
        Self { keymap, xkb, mods: ModifiersState::default() }
    }
}

/// The keymap's xkb form back (Smithay keeps only the text).
fn compile(keymap: &KeymapFile) -> Option<xkb::Keymap> {
    let mut text = None;
    let _ = keymap.with_fd(false, |fd, size| {
        let Ok(file) = fd.try_clone_to_owned().map(std::fs::File::from) else { return };
        let mut buf = vec![0; size];
        if file.read_exact_at(&mut buf, 0).is_ok() {
            buf.retain(|b| *b != 0);
            text = String::from_utf8(buf).ok();
        }
    });
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    xkb::Keymap::new_from_string(&context, text?, xkb::KEYMAP_FORMAT_TEXT_V1, xkb::KEYMAP_COMPILE_NO_FLAGS)
}

impl Helix {
    /// The device's entry, made with its keymap (the seat's when it sent
    /// none).
    fn virtual_keyboard(&mut self, device: &VirtualKeyboardDevice) -> Option<&mut Device> {
        let keymap = match device.keymap() {
            Some(keymap) => keymap,
            None => self.seat.get_keyboard()?.keymap_file(),
        };
        let entry = self.virtual_keyboards.entry(device.clone()).or_insert_with(|| Device::new(keymap.clone()));
        if entry.keymap.id() != keymap.id() {
            *entry = Device::new(keymap);
        }
        Some(entry)
    }
}

impl VirtualKeyboardHandler for Helix {
    fn process_virtual_keyboard_event(&mut self, event: InputEvent<VirtualKeyboardBackend>) {
        match event {
            InputEvent::DeviceAdded { device } | InputEvent::Special(VirtualKeyboardSpecialEvent::KeymapChanged { device }) => {
                self.virtual_keyboard(&device);
            }
            InputEvent::DeviceRemoved { device } => {
                self.virtual_keyboards.remove(&device);
            }
            InputEvent::Keyboard { event } => {
                let Some(entry) = self.virtual_keyboard(&event.device()) else { return };
                let (keymap, mods) = (entry.keymap.clone(), entry.mods);
                let Some(keyboard) = self.seat.get_keyboard() else { return };
                keyboard.input_foreign(self, &keymap, mods, event.key_code(), event.state(), event.time().millis());
            }
            InputEvent::Special(VirtualKeyboardSpecialEvent::Modifiers { device, mods_depressed, mods_latched, mods_locked, group }) => {
                let Some(entry) = self.virtual_keyboard(&device) else { return };
                let Some(xkb) = entry.xkb.as_mut() else { return };
                xkb.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                entry.mods.update_with(xkb);
                let mods = entry.mods;
                let Some(keyboard) = self.seat.get_keyboard() else { return };
                if let Some(focus) = keyboard.current_focus() {
                    focus.modifiers(&self.seat.clone(), self, mods, SERIAL_COUNTER.next_serial());
                }
            }
            _ => {}
        }
    }
}
