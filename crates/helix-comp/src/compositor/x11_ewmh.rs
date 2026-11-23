//! What Helix's own X11 window manager tells X11 apps about the desktop
//! (EWMH root properties and per-window ones that follow Helix's state):
//! the work area, the workspaces and the window each is on, the title bar
//! Helix draws (`_NET_FRAME_EXTENTS`), and the requests apps make about
//! them (helixdesktop/desktop#227).

use smithay::utils::{Logical, Rectangle, SERIAL_COUNTER};
use smithay::wayland::seat::WaylandFocus;

use super::window::{Window, X11Window};
use super::Helix;

impl Helix {
    /// The work area and the workspaces, as X11 apps see them; written
    /// only when they change (this runs on every layer-surface commit).
    pub(super) fn publish_x11_desktop(&mut self) {
        if self.hxwm.is_none() {
            return;
        }
        let outputs: Vec<_> = self.space.outputs().cloned().collect();
        let screens: Vec<Rectangle<i32, Logical>> = outputs.iter().filter_map(|o| self.space.output_geometry(o)).collect();
        let areas: Vec<Rectangle<i32, Logical>> = outputs.iter().filter_map(|o| self.usable_area_of(o)).collect();
        let span = |rects: &[Rectangle<i32, Logical>]| rects.iter().copied().reduce(|a, b| a.merge(b)).unwrap_or_default();
        let (screen, area) = (span(&screens), span(&areas));
        let count = self.workspaces.layout.count.max(1) as u32;
        let active = self.workspaces.layout.active as u32;
        let windows: Vec<Window> = self
            .space
            .elements()
            .chain(self.minimized.iter())
            .chain(self.workspaces.parked())
            .filter(|w| matches!(w.x11_surface(), Some(X11Window::Helix(x)) if !x.is_override_redirect()))
            .cloned()
            .collect();
        let desktops: Vec<(Window, u32)> = windows
            .into_iter()
            .map(|w| {
                let n = if self.on_all_workspaces(&w) { u32::MAX } else { self.workspace_of(&w) as u32 };
                (w, n)
            })
            .collect();
        let Some(xwm) = self.hxwm.as_mut() else { return };
        if let Err(err) = xwm.set_desktops(count, active, screen).and_then(|()| xwm.set_work_area(area, &areas, count)) {
            tracing::debug!(%err, "cannot tell X11 apps about the desktop");
        }
        for (window, n) in desktops {
            if let Some(X11Window::Helix(x)) = window.x11_surface() {
                let _ = x.set_desktop(n);
            }
        }
    }

    /// `_NET_FRAME_EXTENTS` as Helix draws the window now (no title bar
    /// while full screen, or once its Motif hints ask for none).
    pub(super) fn refresh_x11_frame_extents(&self, x11: &X11Window) {
        x11.set_frame_extents(super::frames::x11_frame_top(x11));
    }

    /// `_NET_WM_MOVERESIZE` with the keyboard (EWMH 9 resize, 10 move):
    /// as Move and Resize in the window menu.
    pub(super) fn x11_keyboard_move_resize(&mut self, x11: X11Window, resize: bool) {
        let Some(window) = self.x11_window(&x11) else { return };
        if self.follow.is_some() {
            return;
        }
        self.begin_follow(window, resize);
    }

    /// `_NET_WM_MOVERESIZE` cancel (EWMH 11): a move or resize of this
    /// window going on ends where it is (Escape puts a keyboard one back).
    pub(super) fn x11_cancel_move_resize(&mut self, x11: X11Window) {
        let Some(window) = self.x11_window(&x11) else { return };
        if self.follow.as_ref().is_some_and(|(w, _, _)| *w == window) {
            self.cancel_follow();
            return;
        }
        let pointer = self.seat.get_pointer().unwrap();
        let ours = self.resize.as_ref().is_some_and(|r| r.window == window && !r.finished)
            || pointer.grab_start_data().and_then(|d| d.focus).is_some_and(|(f, _)| f.wl_surface().as_deref() == window.wl_surface().as_deref());
        if ours && pointer.is_grabbed() {
            pointer.unset_grab(self, SERIAL_COUNTER.next_serial(), smithay::backend::input::InputTime::from_millis(0));
        }
    }

    /// `_NET_WM_DESKTOP`: the window asks to go to another workspace
    /// (`u32::MAX`: all of them).
    pub(super) fn x11_move_to_desktop(&mut self, x11: X11Window, desktop: u32) {
        let Some(window) = self.x11_window(&x11) else { return };
        if desktop == u32::MAX {
            self.set_on_all_workspaces(&window, true);
        } else if (desktop as usize) < self.workspaces.layout.count {
            if self.workspaces.is_sticky(&window) {
                self.set_on_all_workspaces(&window, false);
            }
            self.move_to_workspace(&window, desktop as usize);
        }
        self.publish_x11_desktop();
    }

    /// `_NET_CURRENT_DESKTOP`: a pager asks for another workspace.
    pub(super) fn x11_switch_desktop(&mut self, desktop: u32) {
        if (desktop as usize) < self.workspaces.layout.count {
            self.switch_workspace(desktop as usize);
            self.publish_x11_desktop();
        }
    }
}
