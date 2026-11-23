//! Xwayland: runs X11 apps as clients of Helix.
//!
//! The compositor starts an Xwayland server and acts as its X11 window
//! manager (Smithay's `X11Wm`). X11 windows join the same space as Wayland
//! windows and share focus, stacking, move/resize, maximize/fullscreen and
//! the clipboard. Integration follows Smithay's `anvil` example (MIT).

use std::os::fd::OwnedFd;
use std::process::Stdio;

use super::window::{Window, X11Window};
use smithay::input::keyboard::KeyboardTarget;
use smithay::input::pointer::Focus;
use smithay::utils::{Logical, Point, Rectangle, Size, SERIAL_COUNTER};
use smithay::output::Output;
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

pub fn x11_kind(surface: &X11Window) -> X11Kind {
    use smithay::xwayland::xwm::WmWindowType as T;
    match surface.window_type() {
        Some(T::Splash) => X11Kind::Splash,
        Some(T::Notification | T::Dock | T::Tooltip | T::DropdownMenu | T::PopupMenu | T::Combo | T::Dnd) => X11Kind::Placed,
        _ => X11Kind::App,
    }
}

/// How many times a minute Xwayland is started again after it went away.
const XWAYLAND_RESTARTS: usize = 3;

/// Displays tried after Smithay's own search (`:0` to `:32`) found none free.
const FALLBACK_DISPLAYS: std::ops::RangeInclusive<u32> = 33..=99;

impl Helix {
    /// X11 windows that set no cursor of their own show the X root cursor:
    /// the theme's default at `cursor-size`, in X11 pixels (the Xwayland
    /// scale; #171). Set again when the theme, size or scale changes.
    pub fn set_x11_root_cursor(&mut self) {
        let scale = self.xwayland_scale.max(1);
        let key = (self.wm_settings.cursor_theme.clone(), self.wm_settings.cursor_size, scale);
        if self.x11_cursor.as_ref() == Some(&key) {
            return;
        }
        if self.xwm.is_none() && self.hxwm.is_none() {
            return;
        }
        let frame = self.cursors.frame(smithay::input::pointer::CursorIcon::Default, f64::from(scale));
        let (straight, w, h) = &*frame.bgra;
        // X takes the pixels as 32-bit ARGB, B, G, R, A in memory, with
        // premultiplied alpha (Render cursors).
        let pixels: Vec<u8> = straight
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| {
                let a = u32::from(p[3]);
                let pre = |c: u8| ((u32::from(c) * a + 127) / 255) as u8;
                [pre(p[0]), pre(p[1]), pre(p[2]), p[3]]
            })
            .collect();
        let hotspot = (frame.hotspot.0.max(0) as u16, frame.hotspot.1.max(0) as u16);
        let set = match (self.xwm.as_mut(), self.hxwm.as_ref()) {
            (Some(wm), _) => wm.set_cursor(&pixels, Size::from((*w as u16, *h as u16)), hotspot.into()).map_err(|e| e.to_string()),
            (None, Some(wm)) => wm.set_cursor(&pixels, (*w as u16, *h as u16), hotspot).map_err(|e| e.to_string()),
            (None, None) => return,
        };
        match set {
            Ok(()) => {
                tracing::debug!(width = w, height = h, scale, "X11 cursor");
                self.x11_cursor = Some(key);
            }
            Err(err) => tracing::warn!(%err, "cannot set the X11 cursor"),
        }
    }

    /// A splash screen (centered on the output under the pointer) or a
    /// window placing itself (where it asked, kept on its output): shown
    /// without a frame, and focus stays where it is.
    fn map_unfocused_x11(&mut self, window: &Window, surface: &X11Window, kind: X11Kind) {
        let size = surface.last_configure().size;
        let asked = surface.last_configure().loc;
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
    pub fn start_xwayland(&mut self, prefer: Option<u32>, ready: impl FnOnce(&mut Helix, Option<u32>) + 'static) {
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
        // Kept since it went away (`xwayland_gone`): let go just before
        // Xwayland takes it again, on this thread.
        let reserved = self.xwayland_reserved.take();
        let spawn = |display: Option<u32>| XWayland::spawn(&self.display_handle, display, envs.clone(), std::iter::empty::<&str>(), false, Stdio::null(), Stdio::null(), |_| ());
        // Smithay looks for a free display among :0 to :32; when those are
        // all taken (by other X servers or locks that stay behind), the next
        // ones are tried one by one.
        // Again on the display it had (restarted: DISPLAY is exported).
        drop(reserved);
        let mut spawned = match prefer.map(|d| spawn(Some(d))) {
            Some(Ok(spawned)) => Ok(spawned),
            Some(Err(err)) => {
                tracing::warn!(%err, display = prefer, "cannot start Xwayland on its display again");
                spawn(None)
            }
            None => spawn(None),
        };
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
                    let started = if super::xwm::helix_selected() {
                        state.start_helix_xwm(x11_socket, client.clone())
                    } else {
                        match X11Wm::start_wm(state.loop_handle.clone(), &state.display_handle.clone(), x11_socket, client.clone()) {
                            Ok(wm) => {
                                state.xwm = Some(wm);
                                Ok(())
                            }
                            Err(err) => Err(err.to_string()),
                        }
                    };
                    match started {
                        Ok(()) => {
                            state.xdisplay = Some(display_number);
                            state.xwayland_client = Some(client.clone());
                            state.start_x11_settings(display_number);
                            state.set_x11_root_cursor();
                            state.start_x11_ping(display_number);
                            // A Wayland copy made before Xwayland went
                            // away and came back: X11 apps paste it again.
                            for selection in [SelectionTarget::Clipboard, SelectionTarget::Primary] {
                                if let Some(types) = state.x11_offered[offered_index(selection)].clone() {
                                    state.x11_offer_selection(selection, Some(types));
                                }
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
        match result {
            Ok(token) => self.xwayland_source = Some(token),
            Err(err) => tracing::warn!(%err, "cannot watch Xwayland"),
        }
    }

    /// Helix's own window manager (#191) on the connection Xwayland gave
    /// its window manager, and its X events in the event loop.
    fn start_helix_xwm(&mut self, socket: std::os::unix::net::UnixStream, client: smithay::reexports::wayland_server::Client) -> Result<(), String> {
        use smithay::reexports::calloop::generic::Generic;
        use smithay::reexports::calloop::{Interest, Mode, PostAction};
        let (xwm, fd, replies) = super::xwm::Xwm::start(socket, client).map_err(|e| e.to_string())?;
        // Replies come on their own thread; what waited for them runs here.
        let replies = self
            .loop_handle
            .insert_source(replies, |event, _, state| {
                if let smithay::reexports::calloop::channel::Event::Msg(reply) = event {
                    let actions = state.hxwm.as_mut().map(|x| x.resolve(reply)).unwrap_or_default();
                    for action in actions {
                        state.helix_xwm_action(action);
                    }
                    // Events the reply thread read meanwhile.
                    state.dispatch_helix_xwm();
                }
            })
            .map_err(|e| e.to_string())?;
        self.hxwm_replies = Some(replies);
        // What an X11 app's drag source asks of the window manager.
        let (tx, rx) = smithay::reexports::calloop::channel::channel::<super::xwm::DragCmd>();
        self.loop_handle
            .insert_source(rx, |event, _, state| {
                if let smithay::reexports::calloop::channel::Event::Msg(cmd) = event {
                    let actions = state.hxwm.as_mut().map(|x| x.x_drag_cmd(cmd)).unwrap_or_default();
                    for action in actions {
                        state.helix_xwm_action(action);
                    }
                    state.helix_xwm_selection_timer();
                }
            })
            .map_err(|e| e.to_string())?;
        self.x_drag_tx = Some(tx);
        let source = Generic::new(fd, Interest::READ, Mode::Level);
        let token = self
            .loop_handle
            .insert_source(source, |_, _, state| {
                state.dispatch_helix_xwm();
                Ok(PostAction::Continue)
            })
            .map_err(|e| e.to_string())?;
        self.hxwm = Some(xwm);
        self.hxwm_source = Some(token);
        self.dispatch_helix_xwm();
        Ok(())
    }

    /// Whether X11 apps answer, ring the bell and ask for attention, on a
    /// connection of its own.
    fn start_x11_ping(&mut self, display_number: u32) {
        let (answers, received) = smithay::reexports::calloop::channel::channel::<super::x11ping::X11Note>();
        match super::x11ping::X11Ping::start(display_number, answers) {
            Ok(ping) => {
                self.x11ping = Some(ping);
                let _ = self.loop_handle.insert_source(received, |event, _, state| {
                    use smithay::reexports::calloop::channel::Event::Msg;
                    match event {
                        Msg(super::x11ping::X11Note::Pong(id)) => state.x11_pong(id),
                        Msg(super::x11ping::X11Note::PingInfo { window, takes_pings, client }) => state.x11_ping_info(window, takes_pings, client),
                        Msg(super::x11ping::X11Note::Icon(id, icon)) => state.x11_icon_read(id, icon),
                        Msg(super::x11ping::X11Note::IconChanged(id)) => state.x11_icon_changed(id),
                        Msg(super::x11ping::X11Note::Bell(id)) => state.x11_bell(id),
                        Msg(super::x11ping::X11Note::Attention(id, action)) => state.x11_demands_attention(id, action),
                        Msg(super::x11ping::X11Note::MayGrab(id, on)) => {
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
    }

    /// Xwayland went away (it crashed or was killed): forget its windows
    /// and everything tied to it, and start it again on the same display
    /// (#169), at most [`XWAYLAND_RESTARTS`] times a minute.
    fn xwayland_gone(&mut self) {
        tracing::error!("Xwayland is gone; X11 apps have closed");
        let windows: Vec<Window> = self.space.elements().chain(self.minimized_windows()).chain(self.workspaces.parked()).filter(|w| w.x11_surface().is_some()).cloned().collect();
        for window in &windows {
            self.forget_window(window);
        }
        self.x11_selection_cleared(SelectionTarget::Clipboard);
        self.x11_selection_cleared(SelectionTarget::Primary);
        self.xwm = None;
        if let Some(xwm) = self.hxwm.as_mut() {
            xwm.xwayland_gone();
        }
        self.hxwm = None;
        if let Some(token) = self.hxwm_source.take() {
            self.loop_handle.remove(token);
        }
        if let Some(token) = self.hxwm_replies.take() {
            self.loop_handle.remove(token);
        }
        for (_, token) in self.hxwm_watches.drain() {
            self.loop_handle.remove(token);
        }
        self.xwayland_client = None;
        self.x11ping = None;
        self.xsettings = None;
        self.x11_cursor = None;
        self.xwayland_scale = 0;
        let old = self.xdisplay.take();
        // Dropping it gives its display (lock and socket) back.
        if let Some(token) = self.xwayland_source.take() {
            self.loop_handle.remove(token);
        }
        // Kept until Xwayland is started again on it: X11 apps and the
        // session keep DISPLAY, and another X server (another session's
        // Xwayland) could take it in the second before.
        self.xwayland_reserved = old.and_then(DisplayReservation::take);
        helix_comp::xauth::set_current(None);
        self.sync_window_list();

        let now = std::time::Instant::now();
        self.xwayland_restarts.retain(|t| now.duration_since(*t) < std::time::Duration::from_secs(60));
        if self.xwayland_restarts.len() >= XWAYLAND_RESTARTS {
            tracing::error!("Xwayland keeps going away; not starting it again this session");
            self.xwayland_reserved = None;
            return;
        }
        self.xwayland_restarts.push(now);
        let timer = smithay::reexports::calloop::timer::Timer::from_duration(std::time::Duration::from_secs(1));
        let _ = self.loop_handle.insert_source(timer, move |_, _, state| {
            state.start_xwayland(old, move |_, started| match started {
                Some(n) if Some(n) == old => tracing::info!(display = n, "Xwayland is back"),
                Some(n) => tracing::warn!(display = n, before = old, "Xwayland is back on another display; apps started before keep the old DISPLAY"),
                None => tracing::error!("cannot start Xwayland again; X11 apps will not run"),
            });
            smithay::reexports::calloop::timer::TimeoutAction::Drop
        });
    }

    /// The window of `x11`, shown or hidden (minimized, or on another
    /// workspace: a window there withdrawn, destroyed or asking for
    /// something must be found too, #224).
    pub(super) fn x11_window(&self, x11: &X11Window) -> Option<Window> {
        self.space
            .elements()
            .chain(self.minimized_windows())
            .chain(self.workspaces.parked())
            .find(|w| w.x11_surface() == Some(x11))
            .cloned()
    }

    fn smithay_window(&self, surface: &X11Surface) -> Option<Window> {
        self.x11_window(&X11Window::Smithay(surface.clone()))
    }

    // ----- What either X11 window manager asks of Helix ------------------

    /// A client wants its window shown: placed and focused as an app
    /// window, or shown where it asked (splash screens, notifications).
    fn x11_map_request(&mut self, x11: X11Window) {
        let window = Window::new_x11_window(x11.clone());
        let kind = x11_kind(&x11);
        // Maximized or full screen before mapping (EWMH; #167): the app
        // gets that size before it shows, so it doesn't draw at another
        // size first; mapped once it has it.
        // Covering a monitor exactly, undecorated: borderless full screen.
        let legacy = if kind == X11Kind::App && !x11.is_fullscreen() { self.legacy_fullscreen_output(&x11, x11.last_configure()) } else { None };
        let taken_over = kind == X11Kind::App && (x11.is_fullscreen() || x11.is_maximized() || legacy.is_some());
        // Every window is mapped on the X server (and gets `WM_STATE`
        // Normal) only once it is where Helix puts it: an app that reads
        // its position when it is shown (Wine, Java) or when `WM_STATE`
        // says it is managed gets that, not where it was created (#278).
        x11.set_frame_extents(super::frames::frame_top(&window));
        self.publish_x11_desktop();
        if kind != X11Kind::App {
            self.map_unfocused_x11(&window, &x11, kind);
            if self.x11_show(&x11) {
                self.sync_x11_stack();
            } else {
                self.forget_window(&window);
            }
            return;
        }
        let size = x11.last_configure().size;
        // Where the user said it goes (`USPosition`: `-geometry`, a saved
        // session), as mutter: kept, on a monitor as a move after mapping
        // is (#226). Dialogs and transients go over their parent, and a
        // position the program chose (`PPosition`) is Helix's to choose.
        let user_placed = !taken_over && x11.user_position() && x11.transient_for().is_none() && x11.window_type() != Some(smithay::xwayland::xwm::WmWindowType::Dialog);
        let location: Point<i32, Logical> = if user_placed {
            self.constrained_x11_position(&window, x11.last_configure().loc, size)
        } else {
            let (x, y) = self.next_window_position();
            // Room for the title bar Helix draws above it.
            (x, y + super::frames::frame_top(&window)).into()
        };
        if !taken_over {
            let _ = x11.configure(Rectangle::new(location, size));
        }
        tracing::info!(title = x11.title(), class = x11.class(), ?location, ?size, user_placed, "new X11 window");
        self.map_new_window(&window, location);
        if let Some(output) = legacy {
            tracing::info!(output = output.name(), "X11 window covers a monitor: full screen");
            self.wm.legacy_fullscreen.push(window.clone());
            self.fullscreen(&window, Some(output));
        } else if x11.is_fullscreen() {
            self.fullscreen(&window, None);
        } else if x11.is_maximized() {
            self.maximize(&window);
        }
        if !user_placed {
            self.place_new_window(&window);
        }
        if !self.x11_show(&x11) {
            self.forget_window(&window);
            return;
        }
        // Mapped on top of the X stack: stacked as Helix shows it.
        self.sync_x11_stack();
        if x11.is_above() {
            self.set_above(&window, true);
        }
        // Asked to start minimized (WM_HINTS Iconic), as GNOME does.
        if x11.starts_iconic() {
            self.minimize(&window);
        }
        self.x11_initial_desktop(&window, &x11);
    }

    /// The workspace an app window asked to start on (`_NET_WM_DESKTOP`
    /// set, or asked for by message, before mapping; #279), as mutter:
    /// `u32::MAX` puts it on every workspace, one there is puts it there
    /// (hidden, and focus goes back, unless that is the active one), and
    /// any other number leaves it on the active one. `_NET_WM_DESKTOP` is
    /// then written back as it is ([`Helix::publish_x11_desktop`]).
    fn x11_initial_desktop(&mut self, window: &Window, x11: &X11Window) {
        let Some(desktop) = x11.initial_desktop() else { return };
        // The count as it is with this window (dynamic workspaces).
        self.tidy_workspaces();
        if desktop == u32::MAX {
            self.set_on_all_workspaces(window, true);
        } else if (desktop as usize) < self.workspaces.layout.count && desktop as usize != self.workspaces.layout.active {
            tracing::debug!(desktop, "X11 window starts on another workspace");
            self.move_to_workspace(window, desktop as usize);
        }
        self.publish_x11_desktop();
    }

    /// Maps a managed X11 window on the X server (`WM_STATE` Normal, the
    /// client lists); false if it can't be.
    fn x11_show(&mut self, x11: &X11Window) -> bool {
        let mapped = match x11 {
            X11Window::Smithay(s) => s.set_mapped(true).map_err(|e| e.to_string()),
            X11Window::Helix(w) => match self.hxwm.as_mut() {
                Some(xwm) => xwm.set_mapped(w, true).map_err(|e| e.to_string()),
                None => Err("no window manager".into()),
            },
        };
        if let Err(err) = &mapped {
            tracing::warn!(%err, "cannot map X11 window");
        }
        mapped.is_ok()
    }

    fn x11_mapped_override(&mut self, x11: X11Window) {
        let location = x11.last_configure().loc;
        self.space.map_element(Window::new_x11_window(x11), location, false);
    }

    fn x11_unmapped(&mut self, x11: X11Window) {
        if let Some(window) = self.x11_window(&x11) {
            self.forget_window(&window);
        }
        if let X11Window::Smithay(s) = &x11 {
            if !s.is_override_redirect() {
                let _ = s.set_mapped(false);
            }
        }
    }

    fn x11_destroyed(&mut self, x11: X11Window) {
        if let Some(window) = self.x11_window(&x11) {
            self.forget_window(&window);
        }
    }

    fn x11_configure_request(&mut self, x11: X11Window, x: Option<i32>, y: Option<i32>, w: Option<i32>, h: Option<i32>) {
        // Windows that place themselves (notifications, docks) move too.
        if x11_kind(&x11) == X11Kind::Placed {
            let mut geo = x11.last_configure();
            geo.loc = (x.unwrap_or(geo.loc.x), y.unwrap_or(geo.loc.y)).into();
            geo.size = (w.unwrap_or(geo.size.w), h.unwrap_or(geo.size.h)).into();
            let _ = x11.configure(geo);
            if let Some(window) = self.x11_window(&x11) {
                self.space.map_element(window, geo.loc, false);
            }
            return;
        }
        // Apps may resize and move themselves.
        let mut geo = x11.last_configure();
        if let Some(w) = w {
            geo.size.w = w;
        }
        if let Some(h) = h {
            geo.size.h = h;
        }
        // Borderless full screen (#226), as mutter: an undecorated window
        // asking to cover a monitor exactly goes full screen there, and
        // leaves it when it asks for something else.
        if let Some(window) = self.x11_window(&x11) {
            let asked = Rectangle::new((x.unwrap_or(geo.loc.x), y.unwrap_or(geo.loc.y)).into(), geo.size);
            let covers = self.legacy_fullscreen_output(&x11, asked);
            if self.wm.legacy_fullscreen.contains(&window) {
                if covers.is_none() {
                    tracing::info!("X11 window stopped covering its monitor: no longer full screen");
                    self.restore_window(&window);
                }
            } else if let (Some(output), false) = (covers, self.is_any_taken_over(&window)) {
                tracing::info!(output = output.name(), "X11 window covers a monitor: full screen");
                self.wm.legacy_fullscreen.push(window.clone());
                self.fullscreen(&window, Some(output));
                return;
            }
        }
        // Not while fullscreen, maximized or tiled (games set their video
        // mode's size after going fullscreen; as mutter, #165): the app is
        // told its size again, and gets the one it asked for when restored.
        if let Some(window) = self.x11_window(&x11) {
            if let Some((_, restore, _)) = self.restore.iter_mut().find(|(w, _, _)| *w == window) {
                restore.size = geo.size;
                let _ = x11.configure(x11.last_configure());
                return;
            }
        }
        // Within the window's own limits (WM_NORMAL_HINTS).
        if let Some(min) = x11.min_size() {
            geo.size.w = geo.size.w.max(min.w);
            geo.size.h = geo.size.h.max(min.h);
        }
        if let Some(max) = x11.max_size() {
            if max.w > 0 {
                geo.size.w = geo.size.w.min(max.w);
            }
            if max.h > 0 {
                geo.size.h = geo.size.h.min(max.h);
            }
        }
        // A position asked for after it is shown (Wine's SetWindowPos,
        // installers and launchers), as mutter: kept on a monitor, its
        // title bar within the work area (#226). Where a new window first
        // goes is Helix's choice unless the user's (`USPosition`).
        if x.is_some() || y.is_some() {
            if let Some(window) = self.x11_window(&x11).filter(|w| self.presence(w) == super::wm::Presence::Shown) {
                geo.loc = self.constrained_x11_position(&window, (x.unwrap_or(geo.loc.x), y.unwrap_or(geo.loc.y)).into(), geo.size);
                let _ = x11.configure(geo);
                self.space.map_element(window, geo.loc, false);
                return;
            }
        }
        let _ = x11.configure(geo);
    }

    /// The monitor an undecorated X11 app window covering `rect` exactly
    /// is on: how SDL, Unity and Wine titles do borderless full screen.
    fn legacy_fullscreen_output(&self, x11: &X11Window, rect: Rectangle<i32, Logical>) -> Option<Output> {
        // Undecorated: it asks for no title bar (decorates itself).
        if !x11.is_decorated() {
            return None;
        }
        self.space.outputs().find(|o| self.space.output_geometry(o) == Some(rect)).cloned()
    }

    /// Where an X11 window asking to move to `to` goes: on the monitor
    /// there (else its own), its title bar inside the work area and at
    /// least a hand's width of it on the monitor.
    fn constrained_x11_position(&self, window: &Window, to: Point<i32, Logical>, size: smithay::utils::Size<i32, Logical>) -> Point<i32, Logical> {
        const VISIBLE: i32 = 64;
        let output = self.space.output_under(to.to_f64()).next().cloned().or_else(|| self.output_of(window));
        let Some(area) = output.and_then(|o| self.usable_area_of(&o)) else { return to };
        let top = super::frames::frame_top(window);
        let x = to.x.clamp(area.loc.x - size.w + VISIBLE, area.loc.x + area.size.w - VISIBLE);
        let y = to.y.clamp(area.loc.y + top, (area.loc.y + area.size.h - VISIBLE).max(area.loc.y + top));
        (x, y).into()
    }

    /// Override-redirect windows place themselves; follow them.
    fn x11_override_configured(&mut self, x11: X11Window, geometry: Rectangle<i32, Logical>) {
        tracing::debug!(?geometry, "X11 override-redirect window moved");
        if let Some(window) = self.x11_window(&x11) {
            self.space.map_element(window, geometry.loc, false);
        }
    }

    /// A dialog made modal after it was shown (`gtk_window_set_modal`,
    /// Qt's `setWindowModality`): as a Wayland dialog's, focus goes to it
    /// when its parent has it.
    fn x11_modal(&mut self, x11: X11Window, modal: bool) {
        let _ = x11.set_modal(modal);
        if modal {
            if let Some(window) = self.x11_window(&x11) {
                self.modal_window_changed(&window);
            }
        }
    }

    /// An X11 app asks for its window to be activated (`_NET_ACTIVE_WINDOW`,
    /// as `xdg_activation_v1` on Wayland): granted for a pager (`wmctrl -a`,
    /// `xdotool windowactivate`) and for an app moving between its own
    /// windows (the request names the focused window as its active one);
    /// otherwise the window wants attention, as for an activation token
    /// Helix does not grant (`Windows wanting attention`).
    fn x11_activate(&mut self, x11: X11Window, source: u32, current: Option<X11Window>) {
        let Some(window) = self.x11_window(&x11) else { return };
        let focused = self.seat.get_keyboard().and_then(|k| k.current_focus()).and_then(|f| self.window_for_focus(&f));
        let from_focused = current.is_some_and(|active| focused.as_ref().and_then(|w| w.x11_surface()) == Some(&active));
        let pager = source == 2;
        tracing::debug!(pager, from_focused, "X11 activation request");
        if pager || from_focused {
            self.focus_window(&window);
        } else {
            self.set_attention(&window, true);
        }
    }

    fn x11_move(&mut self, x11: X11Window) {
        let Some(window) = self.x11_window(&x11) else { return };
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
                ended: false,
            },
            SERIAL_COUNTER.next_serial(),
            Focus::Clear,
        );
    }

    fn x11_resize(&mut self, x11: X11Window, edges: Edges) {
        let Some(window) = self.x11_window(&x11) else { return };
        let pointer = self.seat.get_pointer().unwrap();
        let Some(start_data) = pointer.grab_start_data() else { return };
        let location = self.space.element_location(&window).unwrap_or_default();
        let initial = Rectangle::new(location, window.geometry().size);
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

    /// The window got its surface: if it was focused when it was mapped
    /// (before it had one), give it the keyboard (properly) now.
    fn x11_associated(&mut self, x11: X11Window) {
        if x11.is_override_redirect() || !x11.is_activated() || self.lock.locked {
            return;
        }
        let target = match x11 {
            X11Window::Smithay(s) => super::focus::KeyboardFocusTarget::X11(s),
            X11Window::Helix(w) => super::focus::KeyboardFocusTarget::XHelix(w),
        };
        let keyboard = self.seat.get_keyboard().unwrap();
        let focus = keyboard.current_focus();
        if focus.is_some() && focus.as_ref() != Some(&target) {
            return;
        }
        // It got focus before it had a surface. A window focused before X
        // mapped it (fullscreen or maximized before being shown) didn't
        // get the X focus either: SetInputFocus fails on an unmapped
        // window. Enter it again now, with its surface.
        match &target {
            // Helix's: its enter gives the X focus and enters the surface,
            // without leaving it first (a leave for a surface that never
            // got an enter, and the X focus via PointerRoot meanwhile).
            super::focus::KeyboardFocusTarget::XHelix(_) if focus.is_some() => {
                let seat = self.seat.clone();
                KeyboardTarget::enter(&target, &seat, self, Vec::new(), SERIAL_COUNTER.next_serial());
                KeyboardTarget::modifiers(&target, &seat, self, keyboard.modifier_state(), SERIAL_COUNTER.next_serial());
            }
            // Smithay's entered the surface on its own once it had one;
            // focus again for the X focus (the app sees one FocusIn: it
            // never had the X focus before).
            _ => {
                keyboard.set_focus(self, Option::<super::focus::KeyboardFocusTarget>::None, SERIAL_COUNTER.next_serial());
                keyboard.set_focus(self, Some(target), SERIAL_COUNTER.next_serial());
            }
        }
    }

    /// An X11 selection owner went away (or Xwayland did): the Wayland
    /// side stops offering what it had.
    fn x11_selection_cleared(&mut self, selection: SelectionTarget) {
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

    /// An X11 app pastes the Wayland selection: write it as `mime_type`
    /// to `fd`, from what Helix itself put there or from the Wayland app.
    pub(super) fn x11_serve_selection(&mut self, selection: SelectionTarget, mime_type: String, fd: OwnedFd) {
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

    /// An X11 app took the selection, with these types: Wayland apps are
    /// offered it.
    fn x11_new_selection(&mut self, selection: SelectionTarget, mime_types: Vec<String>) {
        tracing::info!(?selection, types = mime_types.len(), "selection set by an X11 app");
        self.x11_offered[offered_index(selection)] = None;
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

    /// The Wayland selection changed (a copy, with its types, or none):
    /// X11 apps are offered it, by whichever window manager runs.
    pub(super) fn x11_offer_selection(&mut self, selection: SelectionTarget, types: Option<Vec<String>>) {
        self.x11_offered[offered_index(selection)] = types.clone();
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(err) = xwm.new_selection(selection, types.clone()) {
                tracing::warn!(?err, ?selection, "cannot offer the selection to X11");
            }
        }
        if let Some(xwm) = self.hxwm.as_mut() {
            xwm.set_selection(selection, types);
        }
    }

    /// A Wayland app pastes the selection an X11 app owns.
    pub(super) fn x11_paste_selection(&mut self, selection: SelectionTarget, mime_type: String, fd: OwnedFd) {
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(err) = xwm.send_selection(selection, mime_type, fd) {
                tracing::warn!(?err, "cannot transfer selection from X11");
            }
            return;
        }
        if let Some(xwm) = self.hxwm.as_mut() {
            let actions = xwm.receive_selection(selection, &mime_type, fd);
            for action in actions {
                self.helix_xwm_action(action);
            }
            self.helix_xwm_selection_timer();
        }
    }

    // ----- Helix's own window manager (HELIX_XWM=helix, #191) -------------

    /// Run what Helix's window manager has waiting: X events, turned into
    /// what they ask of Helix.
    pub fn dispatch_helix_xwm(&mut self) {
        loop {
            let Some(xwm) = self.hxwm.as_mut() else { return };
            let events = match xwm.poll() {
                Ok(events) => events,
                Err(err) => {
                    tracing::warn!(%err, "lost the connection to Xwayland");
                    self.xwayland_gone();
                    return;
                }
            };
            let more = !events.is_empty();
            let mut events = events;
            // Each event's requests answered before the next is handled
            // (and events waiting behind replies that came go on).
            loop {
                let actions = self.hxwm.as_mut().map(|x| x.handle(std::mem::take(&mut events))).unwrap_or_default();
                if actions.is_empty() {
                    break;
                }
                for action in actions {
                    self.helix_xwm_action(action);
                }
            }
            self.helix_xwm_selection_timer();
            if !more {
                return;
            }
        }
    }

    /// A resize waits for an X11 app to draw the last one
    /// (`_NET_WM_SYNC_REQUEST`): wake up when it goes anyway, so an app
    /// that never answers can't hold it.
    pub fn helix_xwm_sync_timer(&mut self) {
        use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
        if self.hxwm_sync_timer {
            return;
        }
        let Some(deadline) = self.hxwm.as_ref().and_then(|x| x.sync_deadline()) else { return };
        self.hxwm_sync_timer = true;
        let inserted = self.loop_handle.insert_source(Timer::from_deadline(deadline), |_, _, state| {
            state.hxwm_sync_timer = false;
            if let Some(xwm) = state.hxwm.as_ref() {
                xwm.sync_expired();
            }
            state.helix_xwm_sync_timer();
            TimeoutAction::Drop
        });
        if inserted.is_err() {
            self.hxwm_sync_timer = false;
        }
    }

    /// A surface committed: if it is Xwayland's and carries an
    /// `xwayland_shell_v1` serial, Helix's window manager pairs it.
    pub fn helix_xwm_commit(&mut self, surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface) {
        use smithay::wayland::compositor;
        use smithay::wayland::xwayland_shell::{XWaylandShellCachedState, XWAYLAND_SHELL_ROLE};
        let Some(xwm) = self.hxwm.as_mut() else { return };
        if compositor::get_role(surface) != Some(XWAYLAND_SHELL_ROLE) {
            return;
        }
        let serial = compositor::with_states(surface, |states| states.cached_state.get::<XWaylandShellCachedState>().current().serial);
        let Some(serial) = serial else { return };
        for action in xwm.surface_serial(surface, serial) {
            self.helix_xwm_action(action);
        }
    }

    pub(super) fn helix_xwm_action(&mut self, action: super::xwm::Action) {
        use super::xwm::Action as A;
        let x = X11Window::Helix;
        match action {
            A::MapRequest(w) => self.x11_map_request(x(w)),
            A::MappedOverride(w) => self.x11_mapped_override(x(w)),
            A::Unmapped(w) => self.x11_unmapped(x(w)),
            A::Destroyed(w) => self.x11_destroyed(x(w)),
            A::ConfigureRequest { window, x: cx, y, w, h } => self.x11_configure_request(x(window), cx, y, w, h),
            A::OverrideConfigured(w, rect) => self.x11_override_configured(x(w), rect),
            A::HintsChanged(w) => self.x11_hints_changed(&x(w)),
            A::Associated(w) => self.x11_associated(x(w)),
            A::AppFocused(w) => {
                if let Some(window) = self.x11_window(&x(w)) {
                    if self.space.elements().any(|e| *e == window) {
                        self.focus_window_with(&window, false);
                    }
                }
            }
            A::Fullscreen(w, on) | A::Maximize(w, on) if self.x11_window(&x(w.clone())).is_none() => {
                let _ = on;
            }
            A::Fullscreen(w, on) => {
                let window = self.x11_window(&x(w)).unwrap();
                if on {
                    self.fullscreen(&window, None);
                } else {
                    self.restore_window(&window);
                }
            }
            A::FullscreenMonitors(w) => {
                // Full screen now: over the monitors it named (or its own).
                if let Some(window) = self.x11_window(&x(w)).filter(|w| self.is_taken_over(w, super::wm::Takeover::Fullscreen)) {
                    self.fullscreen(&window, None);
                }
            }
            A::Maximize(w, on) => {
                let window = self.x11_window(&x(w)).unwrap();
                if on {
                    self.maximize(&window);
                } else {
                    self.restore_window(&window);
                }
            }
            A::Above(w, on) => {
                if let Some(window) = self.x11_window(&x(w)) {
                    self.set_above(&window, on);
                }
            }
            A::Modal(w, on) => self.x11_modal(x(w), on),
            A::Activate { window, source, current } => self.x11_activate(x(window), source, current.map(x)),
            A::Minimize(w) => {
                if let Some(window) = self.x11_window(&x(w)) {
                    self.minimize(&window);
                }
            }
            A::Move(w) => self.x11_move(x(w)),
            A::Resize(w, edges) => self.x11_resize(x(w), Edges::from_bits(edges)),
            A::Close(w) => {
                let _ = w.close();
            }
            A::KeyboardMove(w) => self.x11_keyboard_move_resize(x(w), false),
            A::KeyboardResize(w) => self.x11_keyboard_move_resize(x(w), true),
            A::CancelMoveResize(w) => self.x11_cancel_move_resize(x(w)),
            A::RequestFrameExtents(w) | A::DecorationsChanged(w) => self.refresh_x11_frame_extents(&x(w)),
            A::SwitchDesktop(n) => self.x11_switch_desktop(n),
            A::MoveToDesktop(w, n) => self.x11_move_to_desktop(x(w), n),
            A::SelectionOffered(selection, types) => self.x11_new_selection(selection, types),
            A::SelectionCleared(selection) => self.x11_selection_cleared(selection),
            A::SelectionWanted { selection, mime, fd } => self.x11_serve_selection(selection, mime, fd),
            A::Watch { id, fd, write } => {
                use smithay::reexports::calloop::generic::Generic;
                use smithay::reexports::calloop::{Interest, Mode, PostAction};
                let interest = if write { Interest::WRITE } else { Interest::READ };
                let inserted = self.loop_handle.insert_source(Generic::new(fd, interest, Mode::Level), move |_, _, state| {
                    let Some(xwm) = state.hxwm.as_mut() else { return Ok(PostAction::Remove) };
                    let (keep, actions) = xwm.selection_ready(id);
                    if !keep {
                        state.hxwm_watches.remove(&id);
                    }
                    for action in actions {
                        state.helix_xwm_action(action);
                    }
                    state.helix_xwm_selection_timer();
                    Ok(if keep { PostAction::Continue } else { PostAction::Remove })
                });
                match inserted {
                    Ok(token) => {
                        self.hxwm_watches.insert(id, token);
                    }
                    Err(err) => tracing::warn!(%err, "cannot watch a clipboard transfer"),
                }
            }
            A::Unwatch(id) => {
                if let Some(token) = self.hxwm_watches.remove(&id) {
                    self.loop_handle.remove(token);
                }
            }
            A::XDragOffered => self.x_drag_offered(),
            A::XDragGone => self.x_drag_gone(),
        }
    }

    /// An X11 app took XdndSelection: if a button (or a finger) holds an
    /// implicit grab, it drags; Helix carries the drag in the seat's
    /// drag-and-drop grab with the X11 app's source (#191 step 5).
    fn x_drag_offered(&mut self) {
        use smithay::input::dnd::DnDGrab;
        const BTN_LEFT: u32 = 0x110;
        let (Some(tx), Some(_)) = (self.x_drag_tx.clone(), self.hxwm.as_ref()) else { return };
        if self.drag.active() {
            return;
        }
        let seat = self.seat.clone();
        if let Some(pointer) = seat.get_pointer() {
            if let Some(start_data) = pointer.grab_start_data().filter(|d| d.button == BTN_LEFT) {
                let source = self.hxwm.as_mut().unwrap().begin_x_drag(tx);
                let grab = DnDGrab::new_pointer(&self.display_handle, start_data, source, seat.clone());
                pointer.set_grab(self, grab, SERIAL_COUNTER.next_serial(), Focus::Keep);
                self.drag = super::DragState::X11;
                return;
            }
        }
        if let Some(touch) = seat.get_touch() {
            if let Some(start_data) = touch.grab_start_data() {
                let source = self.hxwm.as_mut().unwrap().begin_x_drag(tx);
                let grab = DnDGrab::new_touch(&self.display_handle, start_data, source, seat.clone());
                touch.set_grab(self, grab, SERIAL_COUNTER.next_serial());
                self.drag = super::DragState::X11;
            }
        }
    }

    /// The X11 app's drag is gone (it, or its window, went away): the
    /// seat's grab ends, and the pointer works as before.
    fn x_drag_gone(&mut self) {
        if self.drag != super::DragState::X11 {
            return;
        }
        let seat = self.seat.clone();
        let time = smithay::backend::input::InputTime::now();
        if let Some(pointer) = seat.get_pointer() {
            if pointer.is_grabbed() {
                pointer.unset_grab(self, SERIAL_COUNTER.next_serial(), time);
            }
        }
        if let Some(touch) = seat.get_touch() {
            if touch.is_grabbed() {
                touch.unset_grab(self);
            }
        }
        self.drag_grab_ended();
    }

    /// A clipboard transfer can stall (a hung app on either side): wake
    /// up when the next one gives up.
    pub fn helix_xwm_selection_timer(&mut self) {
        use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
        if self.hxwm_selection_timer {
            return;
        }
        let Some(deadline) = self.hxwm.as_ref().and_then(|x| x.selection_deadline()) else { return };
        self.hxwm_selection_timer = true;
        let inserted = self.loop_handle.insert_source(Timer::from_deadline(deadline), |_, _, state| {
            state.hxwm_selection_timer = false;
            let actions = state.hxwm.as_mut().map(|x| x.selection_expired()).unwrap_or_default();
            for action in actions {
                state.helix_xwm_action(action);
            }
            state.helix_xwm_selection_timer();
            TimeoutAction::Drop
        });
        if inserted.is_err() {
            self.hxwm_selection_timer = false;
        }
    }
}

impl XWaylandShellHandler for Helix {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    fn surface_associated(&mut self, _xwm: XwmId, _wl_surface: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface, surface: X11Surface) {
        self.x11_associated(X11Window::Smithay(surface));
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
            self.x11_hints_changed(&X11Window::new(window));
        }
    }
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        self.x11_map_request(X11Window::Smithay(surface));
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        self.x11_mapped_override(X11Window::Smithay(surface));
    }

    fn unmapped_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        self.x11_unmapped(X11Window::Smithay(surface));
    }

    fn destroyed_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        self.x11_destroyed(X11Window::Smithay(surface));
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
        self.x11_configure_request(X11Window::Smithay(surface), x, y, w.map(|w| w as i32), h.map(|h| h as i32));
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        tracing::debug!(?geometry, or = surface.is_override_redirect(), "X11 configure notify");
        if surface.is_override_redirect() {
            self.x11_override_configured(X11Window::Smithay(surface), geometry);
        }
    }

    fn maximize_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.smithay_window(&surface) {
            self.maximize(&window);
        }
    }

    fn unmaximize_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.smithay_window(&surface) {
            self.restore_window(&window);
        }
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.smithay_window(&surface) {
            self.fullscreen(&window, None);
        }
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.smithay_window(&surface) {
            self.restore_window(&window);
        }
    }

    fn above_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.smithay_window(&surface) {
            self.set_above(&window, true);
        }
    }

    fn unabove_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.smithay_window(&surface) {
            self.set_above(&window, false);
        }
    }

    fn modal_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        self.x11_modal(X11Window::Smithay(surface), true);
    }

    fn unmodal_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        self.x11_modal(X11Window::Smithay(surface), false);
    }

    fn minimize_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.smithay_window(&surface) {
            self.minimize(&window);
        }
    }

    fn active_window_request(&mut self, _xwm: XwmId, surface: X11Surface, source: u32, _timestamp: u32, currently_active: Option<X11Surface>) {
        self.x11_activate(X11Window::Smithay(surface), source, currently_active.map(X11Window::Smithay));
    }

    fn move_request(&mut self, _xwm: XwmId, surface: X11Surface, _button: u32) {
        self.x11_move(X11Window::Smithay(surface));
    }

    fn resize_request(&mut self, _xwm: XwmId, surface: X11Surface, _button: u32, edge: X11ResizeEdge) {
        self.x11_resize(X11Window::Smithay(surface), edges(edge));
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
        self.x11_serve_selection(selection, mime_type, fd);
    }

    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        self.x11_new_selection(selection, mime_types);
    }

    fn disconnected(&mut self, _xwm: XwmId) {
        self.xwayland_gone();
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        self.x11_selection_cleared(selection);
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

/// Where [`Helix::x11_offered`] keeps `selection`'s types.
fn offered_index(selection: SelectionTarget) -> usize {
    match selection {
        SelectionTarget::Clipboard => 0,
        SelectionTarget::Primary => 1,
    }
}

/// An X display kept free (its `/tmp/.X{n}-lock` naming the compositor,
/// as an X server's names itself) while Xwayland is started again on it.
/// X servers, Smithay's included, leave a display whose lock names a live
/// process alone; if the compositor dies the lock is stale and taken
/// over as any other. Let go (the lock removed, if still ours) on drop.
pub struct DisplayReservation {
    display: u32,
    pid: String,
}

impl DisplayReservation {
    fn path(display: u32) -> String {
        format!("/tmp/.X{display}-lock")
    }

    /// Keep display `number`, unless something else has its lock.
    pub fn take(number: u32) -> Option<Self> {
        use std::io::Write;
        let path = Self::path(number);
        let pid = format!("{:>10}\n", std::process::id());
        let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(err) => {
                tracing::warn!(%err, display = number, "cannot keep Xwayland's display while it starts again");
                return None;
            }
        };
        if file.write_all(pid.as_bytes()).is_err() {
            let _ = std::fs::remove_file(&path);
            return None;
        }
        tracing::debug!(display = number, "Xwayland's display kept");
        Some(Self { display: number, pid })
    }
}

impl Drop for DisplayReservation {
    fn drop(&mut self) {
        let path = Self::path(self.display);
        if std::fs::read_to_string(&path).is_ok_and(|now| now == self.pid) {
            let _ = std::fs::remove_file(&path);
        }
    }
}
