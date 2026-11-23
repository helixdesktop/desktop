//! Pairing X11 windows with their Wayland surfaces (Xwayland's
//! `xwayland_shell_v1`): the window is given a serial on the X11
//! connection (`WL_SURFACE_SERIAL`), and its surface commits the same
//! serial on the Wayland one. The two connections aren't ordered, so
//! either comes first (catalog X76). Kept apart from X and Wayland, over
//! any window id and surface, so every order is unit tested here.

use std::collections::HashMap;
use std::hash::Hash;

pub struct Pairing<W, S> {
    /// Each shown window's serial, until it is hidden or goes.
    serials: HashMap<W, u64>,
    /// Each window's surface, once paired.
    paired: HashMap<W, S>,
    /// Serials a window was given before its surface committed one.
    windows: HashMap<u64, W>,
    /// Surfaces whose serial came before the window's.
    surfaces: HashMap<u64, S>,
}

impl<W, S> Default for Pairing<W, S> {
    fn default() -> Self {
        Self { serials: HashMap::new(), paired: HashMap::new(), windows: HashMap::new(), surfaces: HashMap::new() }
    }
}

impl<W: Copy + Eq + Hash, S: Clone + PartialEq> Pairing<W, S> {
    /// Window `window` was given `serial`: the surface it pairs with now,
    /// if that surface committed it already (and is still `alive`). A
    /// window already paired, or a serial another window was given (a
    /// forged or repeated message), is ignored.
    pub fn window_serial(&mut self, window: W, serial: u64, alive: impl Fn(&S) -> bool) -> Option<S> {
        if self.paired.contains_key(&window) || self.serials.values().any(|s| *s == serial) {
            return None;
        }
        self.serials.insert(window, serial);
        self.windows.retain(|_, w| *w != window);
        match self.surfaces.remove(&serial) {
            Some(surface) if alive(&surface) => {
                self.paired.insert(window, surface.clone());
                Some(surface)
            }
            _ => {
                self.windows.insert(serial, window);
                None
            }
        }
    }

    /// `surface` committed `serial` (every commit carries it): the window
    /// it pairs with now, if that window was given it already. A surface
    /// already paired is ignored.
    pub fn surface_serial(&mut self, surface: &S, serial: u64, alive: impl Fn(&S) -> bool) -> Option<W> {
        if self.paired.values().any(|s| s == surface) {
            return None;
        }
        match self.windows.remove(&serial) {
            Some(window) if self.serials.get(&window) == Some(&serial) => {
                self.paired.insert(window, surface.clone());
                Some(window)
            }
            _ => {
                self.surfaces.retain(|_, s| alive(s));
                self.surfaces.insert(serial, surface.clone());
                None
            }
        }
    }

    /// `window` was hidden (Xwayland makes it a new surface and serial
    /// when it is shown again) or is gone: unpaired, its serial forgotten.
    pub fn unpair(&mut self, window: W) {
        self.serials.remove(&window);
        self.paired.remove(&window);
        self.windows.retain(|_, w| *w != window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Surfaces are numbers here; `dead` ones are destroyed.
    fn alive(dead: &'static [u32]) -> impl Fn(&u32) -> bool {
        move |s| !dead.contains(s)
    }

    #[test]
    fn the_serial_first() {
        let mut p = Pairing::<u32, u32>::default();
        assert_eq!(p.window_serial(1, 7, alive(&[])), None);
        assert_eq!(p.surface_serial(&100, 7, alive(&[])), Some(1));
        // Every later commit carries the serial again: nothing new.
        assert_eq!(p.surface_serial(&100, 7, alive(&[])), None);
    }

    #[test]
    fn the_commit_first() {
        let mut p = Pairing::<u32, u32>::default();
        assert_eq!(p.surface_serial(&100, 7, alive(&[])), None);
        assert_eq!(p.surface_serial(&100, 7, alive(&[])), None);
        assert_eq!(p.window_serial(1, 7, alive(&[])), Some(100));
    }

    #[test]
    fn other_serials_do_not_pair() {
        let mut p = Pairing::<u32, u32>::default();
        assert_eq!(p.window_serial(1, 7, alive(&[])), None);
        assert_eq!(p.surface_serial(&100, 8, alive(&[])), None);
        assert_eq!(p.window_serial(2, 8, alive(&[])), Some(100));
        assert_eq!(p.surface_serial(&101, 7, alive(&[])), Some(1));
    }

    #[test]
    fn hidden_before_pairing() {
        let mut p = Pairing::<u32, u32>::default();
        p.window_serial(1, 7, alive(&[]));
        p.unpair(1);
        // The old surface commits late: not the window's any more.
        assert_eq!(p.surface_serial(&100, 7, alive(&[])), None);
        // Shown again: a new serial and surface pair.
        assert_eq!(p.window_serial(1, 8, alive(&[])), None);
        assert_eq!(p.surface_serial(&101, 8, alive(&[])), Some(1));
    }

    #[test]
    fn hidden_after_pairing_pairs_again() {
        let mut p = Pairing::<u32, u32>::default();
        p.window_serial(1, 7, alive(&[]));
        assert_eq!(p.surface_serial(&100, 7, alive(&[])), Some(1));
        p.unpair(1);
        assert_eq!(p.surface_serial(&101, 8, alive(&[])), None);
        assert_eq!(p.window_serial(1, 8, alive(&[])), Some(101));
    }

    #[test]
    fn a_destroyed_surface_is_not_paired() {
        let mut p = Pairing::<u32, u32>::default();
        assert_eq!(p.surface_serial(&100, 7, alive(&[])), None);
        // Destroyed before the window got its serial.
        assert_eq!(p.window_serial(1, 7, alive(&[100])), None);
        // Its replacement still pairs.
        assert_eq!(p.surface_serial(&101, 7, alive(&[100])), Some(1));
    }

    #[test]
    fn destroyed_surfaces_waiting_are_dropped() {
        let mut p = Pairing::<u32, u32>::default();
        p.surface_serial(&100, 7, alive(&[]));
        p.surface_serial(&101, 8, alive(&[100]));
        assert!(!p.surfaces.contains_key(&7), "a destroyed surface was kept waiting");
    }

    #[test]
    fn a_repeated_serial_is_ignored() {
        let mut p = Pairing::<u32, u32>::default();
        p.window_serial(1, 7, alive(&[]));
        // Another window naming the same serial (forged, or repeated).
        assert_eq!(p.window_serial(2, 7, alive(&[])), None);
        assert_eq!(p.surface_serial(&100, 7, alive(&[])), Some(1));
        assert_eq!(p.window_serial(2, 7, alive(&[])), None);
        // A window already paired given another serial.
        assert_eq!(p.surface_serial(&101, 9, alive(&[])), None);
        assert_eq!(p.window_serial(1, 9, alive(&[])), None);
        // A surface already paired committing another window's serial.
        p.window_serial(3, 10, alive(&[]));
        assert_eq!(p.surface_serial(&100, 10, alive(&[])), None);
        assert_eq!(p.surface_serial(&102, 10, alive(&[])), Some(3));
    }

    #[test]
    fn a_new_serial_before_pairing_replaces_the_old() {
        let mut p = Pairing::<u32, u32>::default();
        p.window_serial(1, 7, alive(&[]));
        p.window_serial(1, 8, alive(&[]));
        assert_eq!(p.surface_serial(&100, 7, alive(&[])), None);
        assert_eq!(p.surface_serial(&101, 8, alive(&[])), Some(1));
    }

    #[test]
    fn a_window_gone_is_forgotten() {
        let mut p = Pairing::<u32, u32>::default();
        p.window_serial(1, 7, alive(&[]));
        p.unpair(1);
        // A surface committing its serial after it went waits for a
        // window given that serial.
        assert_eq!(p.surface_serial(&100, 7, alive(&[])), None);
        assert_eq!(p.window_serial(2, 7, alive(&[])), Some(100));
    }
}
