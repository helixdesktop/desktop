//! Xwayland: runs X11 apps as clients of Helix.
//!
//! The compositor starts an Xwayland server and acts as its X11 window
//! manager (Smithay's `X11Wm`). X11 windows join the same space as Wayland
//! windows and share focus, stacking, move/resize, maximize/fullscreen and
//! the clipboard. Integration follows Smithay's `anvil` example (MIT).

use std::os::fd::OwnedFd;
use std::process::Stdio;

use smithay::desktop::Window;
use smithay::input::pointer::Focus;
use smithay::utils::{Logical, Rectangle, Size, SERIAL_COUNTER};
use smithay::wayland::selection::data_device::{
    clear_data_device_selection, current_data_device_selection_userdata,
    request_data_device_client_selection, set_data_device_selection,
};
use smithay::wayland::selection::primary_selection::{
    clear_primary_selection, current_primary_selection_userdata, request_primary_client_selection,
    set_primary_selection,
};
use smithay::wayland::selection::SelectionTarget;
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::xwayland::xwm::{Reorder, ResizeEdge as X11ResizeEdge, XwmId};
use smithay::xwayland::{X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler};

use super::grabs::{Edges, MoveGrab, ResizeGrab, ResizeTracking};
use super::Helix;

/// How Helix treats an X11 window, by its `_NET_WM_WINDOW_TYPE` (#168;
/// as mutter does).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X11Kind {
    /// An app window (normal, dialog, utility, toolbar, menu): framed,
    /// placed by Helix, focused when it opens.
    App,
    /// A splash screen: no frame, centered, not focused.
    Splash,
    /// Notifications, docks, and menus or tooltips that are not
    /// override-redirect: no frame, where the app puts them, not focused.
    Placed,
}

pub fn x11_kind(surface: &X11Surface) -> X11Kind {
    use smithay::xwayland::xwm::WmWindowType as T;
    match surface.window_type() {
        Some(T::Splash) => X11Kind::Splash,
        Some(T::Notification | T::Dock | T::Tooltip | T::DropdownMenu | T::PopupMenu | T::Combo | T::Dnd) => X11Kind::Placed,
        _ => X11Kind::App,
    }
}

/// Displays tried after Smithay's own search (`:0` to `:32`) found none free.
const FALLBACK_DISPLAYS: std::ops::RangeInclusive<u32> = 33..=99;

impl Helix {
    /// A splash screen (centered on the output under the pointer) or a
    /// window placing itself (where it asked, kept on its output): shown
    /// without a frame, and focus stays where it is.
    fn map_unfocused_x11(&mut self, window: &Window, surface: &X11Surface, kind: X11Kind) {
        let size = surface.geometry().size;
        let asked = surface.geometry().loc;
        let output = match kind {
            X11Kind::Placed => self.space.outputs().find(|o| self.space.output_geometry(o).is_some_and(|g| g.contains(asked))).cloned().or_else(|| self.primary()),
            _ => self.pointer_output().or_else(|| self.primary()),
        };
        let area = output.and_then(|o| self.space.output_geometry(&o)).unwrap_or_else(|| Rectangle::new((0, 0).into(), size));
        let location = match kind {
            X11Kind::Splash => smithay::utils::Point::from((area.loc.x + (area.size.w - size.w) / 2, area.loc.y + (area.size.h - size.h) / 2)),
            _ => smithay::utils::Point::from((
                asked.x.clamp(area.loc.x, (area.loc.x + area.size.w - size.w).max(area.loc.x)),
                asked.y.clamp(area.loc.y, (area.loc.y + area.size.h - size.h).max(area.loc.y)),
            )),
        };
        let _ = surface.configure(Rectangle::new(location, size));
        tracing::info!(title = surface.title(), class = surface.class(), ?kind, ?location, ?size, "new X11 window, not focused");
        self.space.map_element(window.clone(), location, false);
        self.restack_above();
    }

    /// At shutdown: give Xwayland's display back (its lock file and
    /// socket). Smithay would on dropping the Xwayland source, but the
    /// event loop's sources are not dropped at exit, and a lock left
    /// behind is one the next X server has to take over (#181).
    pub fn release_x11_display(&self) {
        let Some(number) = self.xdisplay else { return };
        let lock = format!("/tmp/.X{number}-lock");
        // Only ours: the lock holds the pid of the process that took it.
        let ours = std::fs::read_to_string(&lock).is_ok_and(|pid| pid.trim() == std::process::id().to_string());
        if ours {
            let _ = std::fs::remove_file(format!("/tmp/.X11-unix/X{number}"));
            let _ = std::fs::remove_file(&lock);
            tracing::info!(display = number, "released the X display");
        }
    }

    /// Start Xwayland. `ready` runs once it is up (with its display number)
    /// or has failed (`None`).
    pub fn start_xwayland(&mut self, ready: impl FnOnce(&mut Helix, Option<u32>) + 'static) {
        // Only clients with the cookie may connect (and no abstract
        // socket, which any process in the network namespace can reach).
        let auth = match new_auth() {
            Ok(auth) => auth,
            Err(err) => {
                tracing::warn!(%err, "cannot write the X authorization file; X11 apps will not run");
                ready(self, None);
                return;
            }
        };
        // Smithay starts `Xwayland` from PATH with fixed options: a wrapper
        // first in PATH adds `-auth` and the ones the settings ask for.
        let Some(envs) = xwayland_wrapper(&self.wm_settings, &auth.path) else {
            tracing::warn!("cannot prepare Xwayland's options; X11 apps will not run");
            let _ = std::fs::remove_file(&auth.path);
            ready(self, None);
            return;
        };
        let spawn = |display: Option<u32>| XWayland::spawn(&self.display_handle, display, envs.clone(), false, Stdio::null(), Stdio::null(), |_| ());
        // Smithay looks for a free display among :0 to :32; when those are
        // all taken (by other X servers or locks that stay behind), the next
        // ones are tried one by one.
        let mut spawned = spawn(None);
        for display in FALLBACK_DISPLAYS {
            if !matches!(&spawned, Err(err) if err.kind() == std::io::ErrorKind::AddrInUse) {
                break;
            }
            spawned = spawn(Some(display));
        }
        let (xwayland, client) = match spawned {
            Ok(x) => x,
            Err(err) => {
                tracing::warn!(%err, "cannot start Xwayland; X11 apps will not run");
                let _ = std::fs::remove_file(&auth.path);
                ready(self, None);
                return;
            }
        };
        // Now that the display is chosen, entries clients look up by it.
        let contents = helix_comp::xauth::file_contents(&helix_comp::xauth::host_name(), Some(xwayland.display_number()), &auth.cookie);
        if let Err(err) = helix_comp::xauth::write_file(&auth.path, &contents) {
            tracing::warn!(%err, "cannot write the X authorization file");
        }
        helix_comp::xauth::set_current(Some(auth));
        let mut ready = Some(ready);
        let result = self.loop_handle.insert_source(xwayland, move |event, _, state| {
            let display = match event {
                XWaylandEvent::Ready { x11_socket, display_number } => {
                    // The wrapper has run Xwayland by now.
                    if let Some(dir) = wrapper_dir() {
                        let _ = std::fs::remove_dir_all(dir);
                    }
                    match X11Wm::start_wm(state.loop_handle.clone(), x11_socket, client.clone()) {
                        Ok(mut wm) => {
                            let (pixels, w, h) = super::cursor::arrow();
                            // The arrow is BGRA; X wants RGBA.
                            let rgba: Vec<u8> = pixels
                                .as_chunks::<4>()
                                .0
                                .iter()
                                .flat_map(|p| [p[2], p[1], p[0], p[3]])
                                .collect();
                            if let Err(err) = wm.set_cursor(&rgba, Size::from((w as u16, h as u16)), (0u16, 0u16).into()) {
                                tracing::warn!(%err, "cannot set the X11 cursor");
                            }
                            state.xwm = Some(wm);
                            state.xdisplay = Some(display_number);
                            state.xwayland_client = Some(client.clone());
                            state.start_x11_settings(display_number);
                            let (answers, received) = smithay::reexports::calloop::channel::channel::<super::x11ping::X11Note>();
                            match super::x11ping::X11Ping::start(display_number, answers) {
                                Ok(ping) => {
                                    state.x11ping = Some(ping);
                                    let _ = state.loop_handle.insert_source(received, |event, _, state| {
                                        match event {
                                            smithay::reexports::calloop::channel::Event::Msg(super::x11ping::X11Note::Pong(id)) => state.x11_pong(id),
                                            smithay::reexports::calloop::channel::Event::Msg(super::x11ping::X11Note::IconChanged(id)) => state.x11_icon_changed(id),
                                            smithay::reexports::calloop::channel::Event::Msg(super::x11ping::X11Note::Bell(id)) => state.x11_bell(id),
                                            smithay::reexports::calloop::channel::Event::Msg(super::x11ping::X11Note::Attention(id, action)) => state.x11_demands_attention(id, action),
                                            smithay::reexports::calloop::channel::Event::Msg(super::x11ping::X11Note::MayGrab(id, on)) => {
                                                tracing::debug!(id, on, "an X11 window asks to be allowed keyboard grabs");
                                                state.protocols.x11_may_grab.retain(|w| *w != id);
                                                if on {
                                                    state.protocols.x11_may_grab.push(id);
                                                }
                                            }
                                            _ => {}
                                        }
                                    });
                                }
                                Err(err) => tracing::warn!(%err, "cannot check X11 apps are responding"),
                            }
                            tracing::info!(display = display_number, "Xwayland ready");
                            Some(display_number)
                        }
                        Err(err) => {
                            tracing::warn!(%err, "cannot manage Xwayland windows");
                            None
                        }
                    }
                }
                XWaylandEvent::Error => {
                    tracing::warn!("Xwayland failed to start; X11 apps will not run");
                    None
                }
            };
            if let Some(ready) = ready.take() {
                ready(state, display);
            }
        });
        if let Err(err) = result {
            tracing::warn!(%err, "cannot watch Xwayland");
        }
    }

    fn x11_window(&self, surface: &X11Surface) -> Option<Window> {
        self.space
            .elements()
            .chain(self.minimized.iter())
            .find(|w| w.x11_surface() == Some(surface))
            .cloned()
    }
}

impl XWaylandShellHandler for Helix {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    /// The window got its surface: if it was focused when it was mapped
    /// (before it had one), give it the keyboard (properly) now.
    fn surface_associated(&mut self, _xwm: XwmId, wl_surface: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface, surface: X11Surface) {
        if surface.is_override_redirect() || !surface.is_activated() || self.lock.locked {
            return;
        }
        // It got focus while it had no surface: enter again, now with it
        // (the X input focus stays on it).
        let _ = wl_surface;
        let target = super::focus::KeyboardFocusTarget::X11(surface);
        let keyboard = self.seat.get_keyboard().unwrap();
        let focus = keyboard.current_focus();
        if focus.is_none() || focus.as_ref() == Some(&target) {
            keyboard.set_focus(self, Option::<super::focus::KeyboardFocusTarget>::None, SERIAL_COUNTER.next_serial());
            keyboard.set_focus(self, Some(target), SERIAL_COUNTER.next_serial());
        }
    }
}

fn edges(edge: X11ResizeEdge) -> Edges {
    use X11ResizeEdge::*;
    // xdg_toplevel resize edge bits: top 1, bottom 2, left 4, right 8.
    let bits = match edge {
        Top => 1,
        Bottom => 2,
        Left => 4,
        TopLeft => 5,
        BottomLeft => 6,
        Right => 8,
        TopRight => 9,
        BottomRight => 10,
    };
    Edges::from_bits(bits)
}

impl XwmHandler for Helix {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("X11 WM events only arrive while it runs")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn property_notify(&mut self, _xwm: XwmId, window: X11Surface, property: smithay::xwayland::xwm::WmWindowProperty) {
        if matches!(property, smithay::xwayland::xwm::WmWindowProperty::Hints) {
            self.x11_hints_changed(&window);
        }
    }
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Err(err) = surface.set_mapped(true) {
            tracing::warn!(%err, "cannot map X11 window");
            return;
        }
        let window = Window::new_x11_window(surface.clone());
        let kind = x11_kind(&surface);
        if kind != X11Kind::App {
            self.map_unfocused_x11(&window, &surface, kind);
            return;
        }
        let (x, y) = self.next_window_position();
        // Room for the title bar Helix draws above it.
        let location: smithay::utils::Point<i32, Logical> = (x, y + super::frames::frame_top(&window)).into();
        let size = surface.geometry().size;
        let _ = surface.configure(Rectangle::new(location, size));
        tracing::info!(title = surface.title(), class = surface.class(), ?location, ?size, "new X11 window");
        self.map_new_window(&window, location);
        // States the app set before mapping (EWMH; #167).
        if surface.fullscreen_at_map() {
            self.fullscreen(&window, None);
        } else if surface.maximized_at_map() {
            self.maximize(&window);
        }
        self.place_new_window(&window);
        if surface.is_above() {
            self.set_above(&window, true);
        }
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        let location = surface.geometry().loc;
        self.space.map_element(Window::new_x11_window(surface), location, false);
    }

    fn unmapped_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.x11_window(&surface) {
            self.forget_window(&window);
        }
        if !surface.is_override_redirect() {
            let _ = surface.set_mapped(false);
        }
    }

    fn destroyed_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.x11_window(&surface) {
            self.forget_window(&window);
        }
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        // Windows that place themselves (notifications, docks) move too.
        if x11_kind(&surface) == X11Kind::Placed {
            let mut geo = surface.geometry();
            geo.loc = (x.unwrap_or(geo.loc.x), y.unwrap_or(geo.loc.y)).into();
            geo.size = (w.map_or(geo.size.w, |w| w as i32), h.map_or(geo.size.h, |h| h as i32)).into();
            let _ = surface.configure(geo);
            if let Some(window) = self.x11_window(&surface) {
                self.space.map_element(window, geo.loc, false);
            }
            return;
        }
        // Apps may resize themselves; Helix keeps deciding where they go.
        let mut geo = surface.geometry();
        if let Some(w) = w {
            geo.size.w = w as i32;
        }
        if let Some(h) = h {
            geo.size.h = h as i32;
        }
        // Not while fullscreen, maximized or tiled (games set their video
        // mode's size after going fullscreen; as mutter, #165): the app is
        // told its size again, and gets the one it asked for when restored.
        if let Some(window) = self.x11_window(&surface) {
            if let Some((_, restore, _)) = self.restore.iter_mut().find(|(w, _, _)| *w == window) {
                restore.size = geo.size;
                let _ = surface.configure(surface.geometry());
                return;
            }
        }
        // Within the window's own limits (WM_NORMAL_HINTS).
        if let Some(min) = surface.min_size() {
            geo.size.w = geo.size.w.max(min.w);
            geo.size.h = geo.size.h.max(min.h);
        }
        if let Some(max) = surface.max_size() {
            if max.w > 0 {
                geo.size.w = geo.size.w.min(max.w);
            }
            if max.h > 0 {
                geo.size.h = geo.size.h.min(max.h);
            }
        }
        let _ = surface.configure(geo);
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        tracing::debug!(?geometry, or = surface.is_override_redirect(), "X11 configure notify");
        // Override-redirect windows place themselves; follow them.
        if let Some(window) = self.x11_window(&surface) {
            if surface.is_override_redirect() {
                self.space.map_element(window, geometry.loc, false);
            }
        }
    }

    fn maximize_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.x11_window(&surface) {
            self.maximize(&window);
        }
    }

    fn unmaximize_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.x11_window(&surface) {
            self.restore_window(&window);
        }
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.x11_window(&surface) {
            self.fullscreen(&window, None);
        }
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.x11_window(&surface) {
            self.restore_window(&window);
        }
    }

    fn above_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.x11_window(&surface) {
            self.set_above(&window, true);
        }
    }

    fn unabove_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.x11_window(&surface) {
            self.set_above(&window, false);
        }
    }

    /// A dialog made modal after it was shown (`gtk_window_set_modal`,
    /// Qt's `setWindowModality`): as a Wayland dialog's, focus goes to it
    /// when its parent has it.
    fn modal_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        let _ = surface.set_modal(true);
        if let Some(window) = self.x11_window(&surface) {
            self.modal_window_changed(&window);
        }
    }

    fn unmodal_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        let _ = surface.set_modal(false);
    }

    fn minimize_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.x11_window(&surface) {
            self.minimize(&window);
        }
    }

    /// An X11 app asks for its window to be activated (`_NET_ACTIVE_WINDOW`,
    /// as `xdg_activation_v1` on Wayland): granted for a pager (`wmctrl -a`,
    /// `xdotool windowactivate`) and for an app moving between its own
    /// windows (the request names the focused window as its active one);
    /// otherwise the window wants attention, as for an activation token
    /// Helix does not grant (`Windows wanting attention`).
    fn active_window_request(&mut self, _xwm: XwmId, surface: X11Surface, source: u32, _timestamp: u32, currently_active: Option<X11Surface>) {
        let Some(window) = self.x11_window(&surface) else { return };
        let focused = self.seat.get_keyboard().and_then(|k| k.current_focus()).and_then(|f| self.window_for_focus(&f));
        let from_focused = currently_active.is_some_and(|active| focused.as_ref().and_then(|w| w.x11_surface()) == Some(&active));
        let pager = source == 2;
        tracing::debug!(pager, from_focused, "X11 activation request");
        if pager || from_focused {
            self.focus_window(&window);
        } else {
            self.set_attention(&window, true);
        }
    }

    fn move_request(&mut self, _xwm: XwmId, surface: X11Surface, _button: u32) {
        let Some(window) = self.x11_window(&surface) else { return };
        let pointer = self.seat.get_pointer().unwrap();
        let Some(start_data) = pointer.grab_start_data() else { return };
        // Restored under the pointer once it really moves.
        let unmaximize = self.is_any_taken_over(&window);
        let initial_location = self.space.element_location(&window).unwrap_or_default();
        pointer.set_grab(
            self,
            MoveGrab {
                start_data,
                window,
                initial_location,
                follow: false,
                unmaximize,
            },
            SERIAL_COUNTER.next_serial(),
            Focus::Clear,
        );
    }

    fn resize_request(&mut self, _xwm: XwmId, surface: X11Surface, _button: u32, edge: X11ResizeEdge) {
        let Some(window) = self.x11_window(&surface) else { return };
        let pointer = self.seat.get_pointer().unwrap();
        let Some(start_data) = pointer.grab_start_data() else { return };
        let location = self.space.element_location(&window).unwrap_or_default();
        let initial = Rectangle::new(location, window.geometry().size);
        let edges = edges(edge);
        self.resize = Some(ResizeTracking {
            window: window.clone(),
            edges,
            initial,
            finished: false,
        });
        pointer.set_grab(
            self,
            ResizeGrab {
                start_data,
                window,
                edges,
                initial,
                last_size: initial.size,
                follow: false,
            },
            SERIAL_COUNTER.next_serial(),
            Focus::Clear,
        );
    }

    // ----- Clipboard and primary selection between X11 and Wayland -------

    fn allow_selection_access(&mut self, _xwm: XwmId, _selection: SelectionTarget) -> bool {
        // Any X11 client, whatever has focus, as in GNOME (Mutter serves
        // every request): X11 clients can already read one another's
        // input and windows, so a focus rule protected little and broke
        // xclip/xsel run from Wayland terminals and X11 clipboard
        // managers (#172).
        true
    }

    fn send_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_type: String, fd: OwnedFd) {
        if selection == SelectionTarget::Clipboard {
            let current = current_data_device_selection_userdata(&self.seat).map(|r| r.clone());
            match current.as_ref() {
                Some(super::ServerSelection::Image(png)) => {
                    super::capture::serve_image(png.clone(), &mime_type, fd);
                    return;
                }
                Some(super::ServerSelection::Text(text)) => {
                    super::capture::serve_text(text.clone(), &mime_type, fd);
                    return;
                }
                Some(super::ServerSelection::Data { mime_type: offered, data }) => {
                    super::capture::serve_data(data.clone(), offered, &mime_type, fd);
                    return;
                }
                Some(super::ServerSelection::Remote { session }) => {
                    let session = *session;
                    self.start_transfer(session, mime_type, fd);
                    return;
                }
                _ => {}
            }
        }
        let result = match selection {
            SelectionTarget::Clipboard => request_data_device_client_selection(&self.seat, mime_type, fd)
                .map_err(|e| e.to_string()),
            SelectionTarget::Primary => request_primary_client_selection(&self.seat, mime_type, fd)
                .map_err(|e| e.to_string()),
        };
        if let Err(err) = result {
            tracing::warn!(%err, ?selection, "cannot hand the Wayland selection to X11");
        }
    }

    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        tracing::info!(?selection, types = mime_types.len(), "selection set by an X11 app");
        match selection {
            SelectionTarget::Clipboard => {
                set_data_device_selection(&self.display_handle, &self.seat, mime_types.clone(), super::ServerSelection::X11);
                let source = self.x11_clipboard_source();
                self.clipboard_changed(mime_types, source);
            }
            SelectionTarget::Primary => {
                set_primary_selection(&self.display_handle, &self.seat, mime_types, super::ServerSelection::X11)
            }
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        tracing::info!(?selection, "X11 selection cleared");
        match selection {
            SelectionTarget::Clipboard => {
                let x11 = matches!(current_data_device_selection_userdata(&self.seat).as_deref(), Some(super::ServerSelection::X11));
                if x11 {
                    clear_data_device_selection(&self.display_handle, &self.seat);
                    self.clipboard_changed(Vec::new(), super::clipboard::Source::default());
                }
            }
            SelectionTarget::Primary => {
                let x11 = matches!(current_primary_selection_userdata(&self.seat).as_deref(), Some(super::ServerSelection::X11));
                if x11 {
                    clear_primary_selection(&self.display_handle, &self.seat);
                }
            }
        }
    }
}

/// Environment that runs `Xwayland` through a wrapper adding the options of
/// `org.gnome.mutter.wayland` (helixdesktop/spec `contracts/wayland/README.md`,
/// "Xwayland options").
/// A new cookie, in the Xauthority file Xwayland is started with.
fn new_auth() -> std::io::Result<helix_comp::xauth::Auth> {
    use helix_comp::xauth;
    let path = xauth::default_path().ok_or_else(|| std::io::Error::other("no XDG_RUNTIME_DIR"))?;
    let cookie = xauth::Cookie::generate()?;
    xauth::write_file(&path, &xauth::file_contents(&xauth::host_name(), None, &cookie))?;
    Ok(xauth::Auth { path, cookie })
}

fn xwayland_wrapper(settings: &helix_comp::wm_settings::WmSettings, auth: &std::path::Path) -> Option<Vec<(String, String)>> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
    let help = std::process::Command::new("Xwayland").arg("-help").output().ok()?;
    let help = String::from_utf8_lossy(&help.stderr).into_owned() + &String::from_utf8_lossy(&help.stdout);
    let switchable = helix_comp::wm_settings::switchable_extensions(&help);
    let mut args = helix_comp::wm_settings::xwayland_args(settings.xwayland_disable_extension, settings.xwayland_byte_swapped, &switchable);
    args.extend(["-auth".to_owned(), auth.to_str()?.to_owned()]);
    let dir = wrapper_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let script = dir.join("Xwayland");
    if args.iter().any(|a| a.contains('\'')) {
        return None;
    }
    let quoted: Vec<String> = args.iter().map(|a| format!("'{a}'")).collect();
    let body = format!("#!/bin/sh\nPATH=\"$HELIX_XWAYLAND_PATH\" exec Xwayland \"$@\" {}\n", quoted.join(" "));
    std::fs::write(&script, body).ok()?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).ok()?;
    tracing::info!(?args, "Xwayland options");
    Some(vec![
        ("PATH".to_owned(), format!("{}:{path}", dir.display())),
        ("HELIX_XWAYLAND_PATH".to_owned(), path),
    ])
}

/// Where the Xwayland wrapper is written.
fn wrapper_dir() -> Option<std::path::PathBuf> {
    let runtime = std::env::var("XDG_RUNTIME_DIR").ok().filter(|d| !d.is_empty())?;
    Some(std::path::Path::new(&runtime).join(format!("helix-xwayland-{}", std::process::id())))
}
