//! Input for windows of Helix's window manager: events go to the
//! window's Wayland surface (Xwayland passes them on), and a window that
//! gets the keyboard gets the X input focus too.

use smithay::backend::input::{InputTime, KeyState};
use smithay::input::keyboard::{KeyboardTarget, KeysymHandle, ModifiersState};
use smithay::input::Seat;
use smithay::utils::Serial;

use super::window::XWin;
use crate::compositor::Helix;

impl KeyboardTarget<Helix> for XWin {
    fn enter(&self, seat: &Seat<Helix>, data: &mut Helix, keys: Vec<KeysymHandle<'_>>, serial: Serial) {
        if let Err(err) = self.focus() {
            tracing::debug!(%err, "cannot give the X11 window the input focus");
        }
        // Without a surface yet, it gets the keyboard once it is paired
        // (`Helix::x11_associated`).
        if let Some(surface) = self.wl_surface() {
            KeyboardTarget::enter(&surface, seat, data, keys, serial);
        }
    }

    fn leave(&self, seat: &Seat<Helix>, data: &mut Helix, serial: Serial) {
        if let Some(surface) = self.wl_surface() {
            KeyboardTarget::leave(&surface, seat, data, serial);
        }
    }

    fn key(&self, seat: &Seat<Helix>, data: &mut Helix, key: KeysymHandle<'_>, state: KeyState, serial: Serial, time: InputTime) {
        if let Some(surface) = self.wl_surface() {
            KeyboardTarget::key(&surface, seat, data, key, state, serial, time);
        }
    }

    fn modifiers(&self, seat: &Seat<Helix>, data: &mut Helix, modifiers: ModifiersState, serial: Serial) {
        if let Some(surface) = self.wl_surface() {
            KeyboardTarget::modifiers(&surface, seat, data, modifiers, serial);
        }
    }
}
