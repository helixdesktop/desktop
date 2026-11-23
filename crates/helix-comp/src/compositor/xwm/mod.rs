//! Helix's own X11 window manager for Xwayland (helixdesktop/desktop#191),
//! chosen with `HELIX_XWM=helix`; Smithay's stays the default until it
//! does everything.
//!
//! Written from ICCCM, EWMH and Xwayland's `xwayland_shell_v1` serial
//! association. It does not reparent: client windows stay children of the
//! root, are moved and stacked there, and the title bar Helix draws is
//! announced in `_NET_FRAME_EXTENTS`. This part covers becoming the window
//! manager, pairing X11 windows with their Wayland surfaces, mapping and
//! withdrawing, override-redirect windows, configure requests, the
//! window properties Helix reads, EWMH states and activation requests,
//! the client lists, and closing. Selections and drag and drop come
//! later (steps 4 and 5).

mod atoms;
mod focus;
mod props;
pub mod window;

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::AtomicU32;
use std::sync::Arc;

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Resource};
use smithay::utils::{Logical, Rectangle};
use x11rb::connection::Connection;
use x11rb::errors::{ConnectionError, ReplyOrIdError};
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConfigureRequestEvent, ConfigureWindowAux, ConnectionExt as _,
    CreateWindowAux, EventMask, PropMode, StackMode, Window as XId, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::{DefaultStream, RustConnection};
use x11rb::wrapper::ConnectionExt as _;

pub use atoms::Atoms;
pub use window::XWin;

/// Which X11 window manager runs: Smithay's (the default) or Helix's.
pub fn helix_selected() -> bool {
    std::env::var("HELIX_XWM").is_ok_and(|v| v == "helix")
}

/// What the window manager asks of the compositor.
#[derive(Debug)]
pub enum Action {
    /// A client asks for its window to be shown (MapRequest).
    MapRequest(XWin),
    /// An override-redirect window (a menu, a tooltip) was shown.
    MappedOverride(XWin),
    /// Withdrawn, or an override-redirect window hidden.
    Unmapped(XWin),
    Destroyed(XWin),
    /// The window asks to move or resize (logical coordinates).
    ConfigureRequest { window: XWin, x: Option<i32>, y: Option<i32>, w: Option<i32>, h: Option<i32> },
    /// An override-redirect window moved or resized itself.
    OverrideConfigured(XWin, Rectangle<i32, Logical>),
    HintsChanged(XWin),
    /// The window got its Wayland surface.
    Associated(XWin),
    /// `_NET_WM_STATE` requests: (state, on).
    Fullscreen(XWin, bool),
    Maximize(XWin, bool),
    Above(XWin, bool),
    Modal(XWin, bool),
    /// `_NET_ACTIVE_WINDOW`: who asks (EWMH source), and the window the
    /// client says is active now.
    Activate { window: XWin, source: u32, current: Option<XWin> },
    /// `WM_CHANGE_STATE` to Iconic.
    Minimize(XWin),
    /// `_NET_WM_MOVERESIZE`: a move, or a resize by xdg edge bits.
    Move(XWin),
    Resize(XWin, u32),
    /// `_NET_CLOSE_WINDOW`.
    Close(XWin),
}

pub struct Xwm {
    conn: Arc<RustConnection>,
    atoms: Atoms,
    root: XId,
    check: XId,
    client: Client,
    active: Arc<AtomicU32>,
    /// See `Shared::given`.
    given: Arc<AtomicU32>,
    /// See `Shared::focus_seq`.
    focus_seq: Arc<AtomicU32>,
    /// The bits of a resource id that are the client's own: windows of
    /// one X client share the rest (the server's resource-id base).
    id_mask: u32,
    /// Every window the X server told us about, by id.
    windows: HashMap<XId, XWin>,
    /// Serials an X11 window was given before its surface committed one,
    /// and surfaces whose serial came before the window's.
    window_serials: HashMap<u64, XId>,
    surface_serials: HashMap<u64, WlSurface>,
    /// Managed windows, in the order they were mapped, and bottom to top.
    client_list: Vec<XId>,
    stacking: Vec<XId>,
}

impl std::fmt::Debug for Xwm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Xwm").field("root", &self.root).field("windows", &self.windows.len()).finish()
    }
}

/// Why the window manager could not start.
#[derive(Debug)]
pub struct StartError(String);

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<E: std::error::Error> From<E> for StartError {
    fn from(err: E) -> Self {
        Self(err.to_string())
    }
}

impl Xwm {
    /// Become the window manager on the connection Xwayland gave its
    /// window manager. Returns it and a file descriptor that is readable
    /// when X events are waiting.
    pub fn start(socket: UnixStream, client: Client) -> Result<(Self, OwnedFd), StartError> {
        let (stream, _) = DefaultStream::from_unix_stream(socket)?;
        let conn = Arc::new(RustConnection::connect_to_stream(stream, 0)?);
        let atoms = Atoms::new(&*conn)?.reply()?;
        let screen = conn.setup().roots[0].clone();
        let root = screen.root;
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE | EventMask::FOCUS_CHANGE),
        )?
        .check()?;

        // Who the window manager is (EWMH "_NET_SUPPORTING_WM_CHECK"): Java
        // tells window managers apart by it.
        let check = conn.generate_id()?;
        conn.create_window(0, check, root, -1, -1, 1, 1, 0, WindowClass::INPUT_ONLY, x11rb::COPY_FROM_PARENT, &CreateWindowAux::new().override_redirect(1))?;
        conn.change_property32(PropMode::REPLACE, check, atoms._NET_SUPPORTING_WM_CHECK, AtomEnum::WINDOW, &[check])?;
        conn.change_property32(PropMode::REPLACE, root, atoms._NET_SUPPORTING_WM_CHECK, AtomEnum::WINDOW, &[check])?;
        conn.change_property8(PropMode::REPLACE, check, atoms._NET_WM_NAME, atoms.UTF8_STRING, b"Helix")?;
        // The window manager and compositing manager selections (apps use
        // ARGB visuals only with a compositing manager).
        conn.set_selection_owner(check, atoms.WM_S0, x11rb::CURRENT_TIME)?;
        conn.set_selection_owner(check, atoms._NET_WM_CM_S0, x11rb::CURRENT_TIME)?;
        // Xwayland shows a window in a Wayland surface only while it is
        // redirected; the window manager redirects them all (manually: the
        // compositor draws them, from those surfaces).
        {
            use x11rb::protocol::composite::{ConnectionExt as _, Redirect};
            conn.composite_query_version(0, 4)?.reply()?;
            conn.composite_redirect_subwindows(root, Redirect::MANUAL)?;
        }
        // For apps that sync their redraws with resizes
        // (`_NET_WM_SYNC_REQUEST`).
        x11rb::protocol::sync::ConnectionExt::sync_initialize(&*conn, 3, 1)?.reply()?;
        let supported = [
            atoms._NET_SUPPORTED,
            atoms._NET_SUPPORTING_WM_CHECK,
            atoms._NET_WM_NAME,
            atoms._NET_CLIENT_LIST,
            atoms._NET_CLIENT_LIST_STACKING,
            atoms._NET_ACTIVE_WINDOW,
            atoms._NET_CLOSE_WINDOW,
            atoms._NET_FRAME_EXTENTS,
            atoms._NET_WM_MOVERESIZE,
            atoms._NET_WM_STATE,
            atoms._NET_WM_STATE_MODAL,
            atoms._NET_WM_STATE_MAXIMIZED_VERT,
            atoms._NET_WM_STATE_MAXIMIZED_HORZ,
            atoms._NET_WM_STATE_SKIP_TASKBAR,
            atoms._NET_WM_STATE_SKIP_PAGER,
            atoms._NET_WM_STATE_FULLSCREEN,
            atoms._NET_WM_STATE_ABOVE,
            atoms._NET_WM_STATE_FOCUSED,
            atoms._NET_WM_STATE_HIDDEN,
            atoms._NET_WM_WINDOW_TYPE,
            atoms._NET_WM_PID,
            atoms._NET_WM_PING,
            atoms._NET_WM_SYNC_REQUEST,
            atoms._NET_WM_SYNC_REQUEST_COUNTER,
            atoms._NET_WM_STATE_DEMANDS_ATTENTION,
        ];
        conn.change_property32(PropMode::REPLACE, root, atoms._NET_SUPPORTED, AtomEnum::ATOM, &supported)?;
        conn.change_property32(PropMode::REPLACE, root, atoms._NET_CLIENT_LIST, AtomEnum::WINDOW, &[])?;
        conn.change_property32(PropMode::REPLACE, root, atoms._NET_CLIENT_LIST_STACKING, AtomEnum::WINDOW, &[])?;
        conn.change_property32(PropMode::REPLACE, root, atoms._NET_ACTIVE_WINDOW, AtomEnum::WINDOW, &[x11rb::NONE])?;

        let id_mask = conn.setup().resource_id_mask;
        let fd = conn.stream().as_fd().try_clone_to_owned()?;
        let mut xwm = Self {
            conn,
            atoms,
            root,
            check,
            client,
            active: Arc::new(AtomicU32::new(x11rb::NONE)),
            given: Arc::new(AtomicU32::new(x11rb::NONE)),
            focus_seq: Arc::new(AtomicU32::new(0)),
            id_mask,
            windows: HashMap::new(),
            window_serials: HashMap::new(),
            surface_serials: HashMap::new(),
            client_list: Vec::new(),
            stacking: Vec::new(),
        };
        // Windows made before the window manager started.
        let conn = xwm.conn.clone();
        let tree = conn.query_tree(root)?.reply()?;
        for child in tree.children {
            if child == check {
                continue;
            }
            if let Ok(attrs) = conn.get_window_attributes(child)?.reply() {
                if attrs.class == WindowClass::INPUT_ONLY {
                    continue;
                }
                let geo = conn.get_geometry(child)?.reply()?;
                xwm.add(child, attrs.override_redirect, geo.x.into(), geo.y.into(), geo.width.into(), geo.height.into());
            }
        }
        xwm.conn.flush()?;
        tracing::info!("Helix's X11 window manager is running");
        Ok((xwm, fd))
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The events waiting, without blocking. An error means Xwayland is
    /// gone.
    pub fn poll(&mut self) -> Result<Vec<Event>, ConnectionError> {
        let mut events = Vec::new();
        while let Some(event) = self.conn.poll_for_event()? {
            events.push(event);
        }
        Ok(events)
    }

    pub fn flush(&self) {
        let _ = self.conn.flush();
    }

    fn add(&mut self, id: XId, override_redirect: bool, x: i32, y: i32, w: i32, h: i32) -> XWin {
        let probe = XWin::new(id, &self.conn, self.atoms, self.root, Some(self.client.clone()), (self.active.clone(), self.given.clone(), self.focus_seq.clone()), override_redirect, Rectangle::default());
        let geometry = Rectangle::new((probe.x_to_logical(x), probe.x_to_logical(y)).into(), (probe.x_to_logical(w), probe.x_to_logical(h)).into());
        probe.state().last_configure = geometry;
        self.windows.insert(id, probe.clone());
        probe
    }

    /// The window with this id, if Helix's window manager knows it.
    pub fn window(&self, id: XId) -> Option<&XWin> {
        self.windows.get(&id)
    }

    /// Handles one X event.
    pub fn handle(&mut self, event: Event) -> Vec<Action> {
        match self.handle_inner(event) {
            Ok(actions) => actions,
            Err(err) => {
                tracing::debug!(%err, "X11 request failed");
                Vec::new()
            }
        }
    }

    fn handle_inner(&mut self, event: Event) -> Result<Vec<Action>, ReplyOrIdError> {
        let mut actions = Vec::new();
        tracing::trace!(?event, "X11 event");
        match event {
            Event::CreateNotify(e) if e.parent == self.root && e.window != self.check => {
                if !self.windows.contains_key(&e.window) {
                    self.add(e.window, e.override_redirect, e.x.into(), e.y.into(), e.width.into(), e.height.into());
                }
            }
            Event::DestroyNotify(e) => {
                if let Some(win) = self.windows.remove(&e.window) {
                    win.state().destroyed = true;
                    let alarm = std::mem::take(&mut win.state().sync.alarm);
                    if alarm != 0 {
                        x11rb::protocol::sync::ConnectionExt::sync_destroy_alarm(&*self.conn, alarm)?;
                    }
                    self.unlist(e.window)?;
                    self.window_serials.retain(|_, w| *w != e.window);
                    actions.push(Action::Destroyed(win));
                }
            }
            Event::MapRequest(e) => self.map_request(e.window, &mut actions)?,
            Event::MapNotify(e) => {
                let Some(win) = self.windows.get(&e.window).cloned() else { return Ok(actions) };
                // The flag counts as it is when the window is shown: clients
                // change it after creating the window, both ways.
                let was_override = {
                    let mut state = win.state();
                    state.viewable = true;
                    let was = state.override_redirect;
                    state.override_redirect = e.override_redirect;
                    was
                };
                tracing::debug!(window = e.window, override_redirect = e.override_redirect, "X11 window shown");
                if e.override_redirect {
                    let geo = self.conn.get_geometry(e.window)?.reply()?;
                    win.state().last_configure = Rectangle::new((win.x_to_logical(geo.x.into()), win.x_to_logical(geo.y.into())).into(), (win.x_to_logical(geo.width.into()), win.x_to_logical(geo.height.into())).into());
                    props::read_all(&self.conn, &self.atoms, &win)?;
                    actions.push(Action::MappedOverride(win));
                } else if !win.state().managed && !win.state().input_only {
                    // Shown but not managed: made managed after it was
                    // created and mapped while it was override-redirect, or
                    // shown and withdrawn faster than the window manager
                    // caught up (the withdrawal handled after the last map,
                    // Zoom's toolbar). It is shown, so it is managed.
                    let _ = was_override;
                    self.manage(&win, &mut actions)?;
                }
            }
            Event::UnmapNotify(e) => {
                let Some(win) = self.windows.get(&e.window).cloned() else { return Ok(actions) };
                if e.response_type & 0x80 != 0 {
                    // A synthetic UnmapNotify is a withdraw request (ICCCM
                    // 4.1.4), also when it comes before the window manager's
                    // own map shows (mapped and withdrawn at once); after
                    // the client unmapped the window itself it is no news.
                    let pending = {
                        let s = win.state();
                        (s.managed || s.viewable) && !s.override_redirect
                    };
                    if pending {
                        self.conn.unmap_window(e.window)?;
                        self.conn.flush()?;
                    }
                    return Ok(actions);
                }
                if e.event != self.root {
                    return Ok(actions);
                }
                self.unmapped(&win, &mut actions)?;
            }
            Event::ConfigureRequest(e) => self.configure_request(e, &mut actions)?,
            Event::ConfigureNotify(e) if e.event == self.root => {
                if let Some(win) = self.windows.get(&e.window).cloned() {
                    let rect = Rectangle::new((win.x_to_logical(e.x.into()), win.x_to_logical(e.y.into())).into(), (win.x_to_logical(e.width.into()), win.x_to_logical(e.height.into())).into());
                    let (or, viewable) = {
                        let mut state = win.state();
                        if state.override_redirect || !state.managed {
                            state.last_configure = rect;
                        }
                        (state.override_redirect, state.viewable)
                    };
                    if or && viewable {
                        actions.push(Action::OverrideConfigured(win, rect));
                    }
                }
            }
            Event::PropertyNotify(e) => {
                let Some(win) = self.windows.get(&e.window).cloned() else { return Ok(actions) };
                props::read(&self.conn, &self.atoms, &win, e.atom)?;
                if e.atom == self.atoms.WM_TRANSIENT_FOR {
                    self.break_transient_cycle(&win);
                }
                if e.atom == self.atoms.WM_HINTS {
                    actions.push(Action::HintsChanged(win));
                }
            }
            Event::ClientMessage(e) => self.client_message(e, &mut actions)?,
            Event::FocusIn(e) => self.focus_in(e)?,
            Event::SyncAlarmNotify(e) => {
                if let Some(win) = self.windows.values().find(|w| w.state().sync.alarm == e.alarm).cloned() {
                    win.sync_done()?;
                }
            }
            _ => {}
        }
        Ok(actions)
    }

    /// The X focus moved (ICCCM 4.1.7, EWMH `_NET_ACTIVE_WINDOW`). Focus
    /// events of keyboard grabs (menus) and of the window under the
    /// pointer while the focus follows it (detail Pointer) are no
    /// activation. Focus on nothing or on the pointer root: no X11 window
    /// is active (Wine locks the mouse on the active window). An app
    /// moving the focus among its own windows (Steam, Zoom) keeps it
    /// there, and that window is the active one.
    fn focus_in(&mut self, e: x11rb::protocol::xproto::FocusInEvent) -> Result<(), ReplyOrIdError> {
        use x11rb::protocol::xproto::{NotifyDetail, NotifyMode};
        if matches!(e.mode, NotifyMode::GRAB | NotifyMode::UNGRAB) || e.detail == NotifyDetail::POINTER {
            return Ok(());
        }
        // From before the window manager's own last focus change (the
        // server's revert when the focused window went away, handled
        // after Helix focused the next one): stale.
        let ours = self.focus_seq.load(std::sync::atomic::Ordering::Relaxed) as u16;
        if (ours.wrapping_sub(e.sequence) as i16) > 0 {
            return Ok(());
        }
        if e.event == self.root {
            if matches!(e.detail, NotifyDetail::NONE | NotifyDetail::POINTER_ROOT) {
                self.set_active(x11rb::NONE)?;
            }
            return Ok(());
        }
        let Some(win) = self.windows.get(&e.event) else { return Ok(()) };
        if win.state().override_redirect || !win.state().managed {
            return Ok(());
        }
        let given = self.given.load(std::sync::atomic::Ordering::Relaxed);
        if e.event == given {
            return Ok(());
        }
        let mask = !self.id_mask;
        if given != x11rb::NONE && given & mask == e.event & mask {
            self.given.store(e.event, std::sync::atomic::Ordering::Relaxed);
            self.set_active(e.event)?;
        }
        Ok(())
    }

    /// `_NET_ACTIVE_WINDOW`, rewritten only when it changes.
    fn set_active(&self, id: XId) -> Result<(), ReplyOrIdError> {
        if self.active.swap(id, std::sync::atomic::Ordering::Relaxed) != id {
            self.conn.change_property32(PropMode::REPLACE, self.root, self.atoms._NET_ACTIVE_WINDOW, AtomEnum::WINDOW, &[id])?;
            self.conn.flush()?;
        }
        Ok(())
    }

    /// The keyboard went to something that isn't one of its windows (a
    /// Wayland window, or nothing): the X focus goes to the pointer root,
    /// not to None, so X11 apps can still grab the keyboard (dmenu).
    pub fn focus_elsewhere(&self) {
        if self.given.swap(x11rb::NONE, std::sync::atomic::Ordering::Relaxed) == x11rb::NONE {
            return;
        }
        let pointer_root = u32::from(x11rb::protocol::xproto::InputFocus::POINTER_ROOT);
        if let Ok(cookie) = self.conn.set_input_focus(x11rb::protocol::xproto::InputFocus::POINTER_ROOT, pointer_root, x11rb::CURRENT_TIME) {
            self.focus_seq.store(cookie.sequence_number() as u32, std::sync::atomic::Ordering::Relaxed);
        }
        let _ = self.conn.flush();
    }

    fn map_request(&mut self, id: XId, actions: &mut Vec<Action>) -> Result<(), ReplyOrIdError> {
        let attrs = self.conn.get_window_attributes(id)?.reply()?;
        // Input-only windows show nothing and are not managed.
        if attrs.class == WindowClass::INPUT_ONLY {
            if let Some(w) = self.windows.get(&id) {
                w.state().input_only = true;
            }
            self.conn.map_window(id)?;
            self.conn.flush()?;
            return Ok(());
        }
        let win = match self.windows.get(&id) {
            Some(w) => w.clone(),
            None => {
                let geo = self.conn.get_geometry(id)?.reply()?;
                self.add(id, attrs.override_redirect, geo.x.into(), geo.y.into(), geo.width.into(), geo.height.into())
            }
        };
        win.state().override_redirect = attrs.override_redirect;
        if attrs.override_redirect {
            self.conn.map_window(id)?;
            self.conn.flush()?;
            return Ok(());
        }
        if win.state().managed {
            // Asked again while shown (some toolkits do): it is already.
            self.conn.map_window(id)?;
            self.conn.flush()?;
            return Ok(());
        }
        self.manage(&win, actions)
    }

    /// Starts managing a window the client wants shown: its properties
    /// (and the states it set before mapping), then the compositor
    /// decides where it goes and maps it ([`XWin::set_mapped`]).
    fn manage(&mut self, win: &XWin, actions: &mut Vec<Action>) -> Result<(), ReplyOrIdError> {
        self.conn.change_window_attributes(win.window_id(), &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE | EventMask::FOCUS_CHANGE))?;
        props::read_all(&self.conn, &self.atoms, win)?;
        self.break_transient_cycle(win);
        win.state().managed = true;
        actions.push(Action::MapRequest(win.clone()));
        Ok(())
    }

    /// The compositor shows a managed window (true) or stops (false).
    pub fn set_mapped(&mut self, win: &XWin, mapped: bool) -> Result<(), ConnectionError> {
        let id = win.window_id();
        if mapped {
            self.conn.change_property32(PropMode::REPLACE, id, self.atoms.WM_STATE, self.atoms.WM_STATE, &[1, x11rb::NONE])?;
            let states: Vec<u32> = win.state().net_state.iter().copied().collect();
            self.conn.change_property32(PropMode::REPLACE, id, self.atoms._NET_WM_STATE, AtomEnum::ATOM, &states)?;
            self.conn.map_window(id)?;
            if !self.client_list.contains(&id) {
                self.client_list.push(id);
            }
            self.stacking.retain(|w| *w != id);
            self.stacking.push(id);
            self.publish_lists()?;
        } else {
            self.conn.unmap_window(id)?;
        }
        self.conn.flush()
    }

    fn unmapped(&mut self, win: &XWin, actions: &mut Vec<Action>) -> Result<(), ReplyOrIdError> {
        let id = win.window_id();
        let (managed, or) = {
            let mut state = win.state();
            let was = (state.managed, state.override_redirect);
            state.viewable = false;
            state.managed = false;
            // Xwayland makes a new surface (and serial) when it is shown again.
            state.wl_surface = None;
            state.serial = None;
            was
        };
        if managed {
            // Withdrawn (ICCCM 4.1.3.1), and its EWMH states go (EWMH
            // "_NET_WM_STATE": removed when the window is withdrawn).
            self.conn.change_property32(PropMode::REPLACE, id, self.atoms.WM_STATE, self.atoms.WM_STATE, &[0, x11rb::NONE])?;
            self.conn.delete_property(id, self.atoms._NET_WM_STATE)?;
            win.state().net_state.clear();
            self.unlist(id)?;
            actions.push(Action::Unmapped(win.clone()));
        } else if or {
            actions.push(Action::Unmapped(win.clone()));
        }
        self.conn.flush()?;
        Ok(())
    }

    fn unlist(&mut self, id: XId) -> Result<(), ConnectionError> {
        let before = self.client_list.len() + self.stacking.len();
        self.client_list.retain(|w| *w != id);
        self.stacking.retain(|w| *w != id);
        if before != self.client_list.len() + self.stacking.len() {
            self.publish_lists()?;
        }
        Ok(())
    }

    fn publish_lists(&self) -> Result<(), ConnectionError> {
        self.conn.change_property32(PropMode::REPLACE, self.root, self.atoms._NET_CLIENT_LIST, AtomEnum::WINDOW, &self.client_list)?;
        self.conn.change_property32(PropMode::REPLACE, self.root, self.atoms._NET_CLIENT_LIST_STACKING, AtomEnum::WINDOW, &self.stacking)?;
        Ok(())
    }

    /// Puts a managed window on top of the other managed ones: right
    /// above the topmost, so override-redirect menus stay above it.
    pub fn raise(&mut self, win: &XWin) -> Result<(), ConnectionError> {
        let id = win.window_id();
        // Not managed, or on top already: nothing changes (no restack for
        // pagers to wake up to).
        if !self.stacking.contains(&id) || self.stacking.last() == Some(&id) {
            return Ok(());
        }
        let top = self.stacking.iter().rev().find(|w| **w != id).copied();
        let aux = match top {
            Some(sibling) => ConfigureWindowAux::new().sibling(sibling).stack_mode(StackMode::ABOVE),
            None => ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        };
        self.conn.configure_window(id, &aux)?;
        self.stacking.retain(|w| *w != id);
        self.stacking.push(id);
        self.publish_lists()?;
        self.conn.flush()
    }

    fn configure_request(&mut self, e: ConfigureRequestEvent, actions: &mut Vec<Action>) -> Result<(), ReplyOrIdError> {
        use x11rb::protocol::xproto::ConfigWindow as C;
        let Some(win) = self.windows.get(&e.window).cloned() else {
            // Not one we know: let it have what it asks.
            self.conn.configure_window(e.window, &ConfigureWindowAux::from_configure_request(&e))?;
            self.conn.flush()?;
            return Ok(());
        };
        let (managed, or) = {
            let s = win.state();
            (s.managed, s.override_redirect)
        };
        if !managed || or {
            // Not shown yet (or placing itself): it may set what it likes;
            // Helix decides once it is mapped. Stacking requests of
            // unmanaged windows go through too.
            self.conn.configure_window(e.window, &ConfigureWindowAux::from_configure_request(&e))?;
            self.conn.flush()?;
            let mut state = win.state();
            let mut rect = state.last_configure;
            if e.value_mask.contains(C::X) {
                rect.loc.x = win.x_to_logical(e.x.into());
            }
            if e.value_mask.contains(C::Y) {
                rect.loc.y = win.x_to_logical(e.y.into());
            }
            if e.value_mask.contains(C::WIDTH) {
                rect.size.w = win.x_to_logical(e.width.into());
            }
            if e.value_mask.contains(C::HEIGHT) {
                rect.size.h = win.x_to_logical(e.height.into());
            }
            state.last_configure = rect;
            return Ok(());
        }
        let pick = |flag: C, v: i32| e.value_mask.contains(flag).then(|| win.x_to_logical(v));
        let (x, y, w, h) = (pick(C::X, e.x.into()), pick(C::Y, e.y.into()), pick(C::WIDTH, e.width.into()), pick(C::HEIGHT, e.height.into()));
        if x.is_none() && y.is_none() && w.is_none() && h.is_none() {
            // Only a stacking change, or nothing: answered all the same
            // (ICCCM 4.1.5), with where the window is.
            win.configure(None)?;
            return Ok(());
        }
        actions.push(Action::ConfigureRequest { window: win, x, y, w, h });
        Ok(())
    }

    fn client_message(&mut self, e: x11rb::protocol::xproto::ClientMessageEvent, actions: &mut Vec<Action>) -> Result<(), ReplyOrIdError> {
        let a = self.atoms;
        let Some(win) = self.windows.get(&e.window).cloned() else { return Ok(()) };
        let data = e.data.as_data32();
        if e.type_ == a.WL_SURFACE_SERIAL {
            let serial = u64::from(data[0]) | (u64::from(data[1]) << 32);
            self.window_serial(&win, serial, actions);
        } else if e.type_ == a._NET_WM_STATE {
            let managed = win.state().managed;
            let action = data[0];
            for atom in [data[1], data[2]] {
                if atom == x11rb::NONE {
                    continue;
                }
                let on = match action {
                    0 => false,
                    1 => true,
                    2 => !win.state().net_state.contains(&atom),
                    _ => continue,
                };
                if !managed {
                    // Before it is shown the client's word goes as it is.
                    if on {
                        win.state().net_state.insert(atom);
                    } else {
                        win.state().net_state.remove(&atom);
                    }
                    continue;
                }
                if atom == a._NET_WM_STATE_FULLSCREEN {
                    actions.push(Action::Fullscreen(win.clone(), on));
                } else if atom == a._NET_WM_STATE_MAXIMIZED_HORZ || atom == a._NET_WM_STATE_MAXIMIZED_VERT {
                    // Both in one message count once.
                    if !actions.iter().any(|x| matches!(x, Action::Maximize(w, _) if *w == win)) {
                        actions.push(Action::Maximize(win.clone(), on));
                    }
                } else if atom == a._NET_WM_STATE_ABOVE {
                    actions.push(Action::Above(win.clone(), on));
                } else if atom == a._NET_WM_STATE_MODAL {
                    actions.push(Action::Modal(win.clone(), on));
                } else if atom == a._NET_WM_STATE_SKIP_TASKBAR || atom == a._NET_WM_STATE_SKIP_PAGER {
                    if on {
                        win.change_state(&[atom], &[])?;
                    } else {
                        win.change_state(&[], &[atom])?;
                    }
                }
            }
        } else if e.type_ == a._NET_ACTIVE_WINDOW {
            let current = self.windows.get(&data[2]).cloned();
            actions.push(Action::Activate { window: win, source: data[0], current });
        } else if e.type_ == a.WM_CHANGE_STATE {
            if data[0] == 3 && win.state().managed {
                actions.push(Action::Minimize(win));
            }
        } else if e.type_ == a._NET_WM_MOVERESIZE {
            // EWMH directions: 0 top-left … 7 left, 8 move; keyboard ones and
            // cancel (9–11) are not taken.
            let edges = [5, 1, 9, 8, 10, 2, 6, 4];
            match data[2] {
                8 => actions.push(Action::Move(win)),
                d @ 0..=7 => actions.push(Action::Resize(win, edges[d as usize])),
                _ => {}
            }
        } else if e.type_ == a._NET_CLOSE_WINDOW {
            actions.push(Action::Close(win));
        }
        Ok(())
    }

    /// A window was given a serial (Xwayland's `WL_SURFACE_SERIAL`): pair
    /// it with the surface that commits the same one, whichever comes
    /// first. A serial is used once; another window naming one already
    /// paired (a forged message) is ignored.
    fn window_serial(&mut self, win: &XWin, serial: u64, actions: &mut Vec<Action>) {
        if win.state().wl_surface.is_some() || self.windows.values().any(|w| w.state().serial == Some(serial)) {
            tracing::debug!(window = win.window_id(), serial, "ignoring a serial already used");
            return;
        }
        win.state().serial = Some(serial);
        match self.surface_serials.remove(&serial) {
            Some(surface) if surface.is_alive() => self.associate(win, surface, actions),
            _ => {
                self.window_serials.retain(|_, w| *w != win.window_id());
                self.window_serials.insert(serial, win.window_id());
            }
        }
    }

    /// A surface of Xwayland's committed with a serial
    /// (`xwayland_surface_v1.set_serial`).
    pub fn surface_serial(&mut self, surface: &WlSurface, serial: u64) -> Vec<Action> {
        let mut actions = Vec::new();
        if !window::from_client(surface, &self.client) {
            return actions;
        }
        if self.windows.values().any(|w| w.state().wl_surface.as_ref() == Some(surface)) {
            return actions;
        }
        match self.window_serials.remove(&serial).and_then(|id| self.windows.get(&id).cloned()) {
            Some(win) if win.state().serial == Some(serial) => self.associate(&win, surface.clone(), &mut actions),
            _ => {
                self.surface_serials.retain(|_, s| s.is_alive());
                self.surface_serials.insert(serial, surface.clone());
            }
        }
        actions
    }

    fn associate(&mut self, win: &XWin, surface: WlSurface, actions: &mut Vec<Action>) {
        tracing::debug!(window = win.window_id(), surface = ?surface.id(), "X11 window paired with its surface");
        win.state().wl_surface = Some(surface);
        actions.push(Action::Associated(win.clone()));
    }

    /// `WM_TRANSIENT_FOR` chains that loop back are cut where they close.
    fn break_transient_cycle(&self, win: &XWin) {
        let mut seen = vec![win.window_id()];
        let mut next = win.transient_for();
        while let Some(id) = next {
            if seen.contains(&id) {
                tracing::debug!(window = win.window_id(), "WM_TRANSIENT_FOR loops; ignored");
                win.state().transient_for = None;
                return;
            }
            seen.push(id);
            next = self.windows.get(&id).and_then(|w| w.transient_for());
        }
    }

    /// Gives a window the X input focus (as its input model asks).
    pub fn focus(&self, win: &XWin) {
        if let Err(err) = win.focus() {
            tracing::debug!(%err, "cannot focus the X11 window");
        }
    }

    /// The root window's cursor, which X11 windows without one of their
    /// own show: `pixels` as 32-bit ARGB (B, G, R, A in memory,
    /// premultiplied), `size` and `hotspot` in X pixels (#171).
    pub fn set_cursor(&self, pixels: &[u8], size: (u16, u16), hotspot: (u16, u16)) -> Result<(), ReplyOrIdError> {
        use x11rb::protocol::render::{ConnectionExt as _, CreatePictureAux, PictType};
        use x11rb::protocol::xproto::{CreateGCAux, ImageFormat};
        let (w, h) = size;
        if pixels.len() < usize::from(w) * usize::from(h) * 4 || w == 0 || h == 0 {
            return Ok(());
        }
        let formats = self.conn.render_query_pict_formats()?.reply()?;
        let Some(format) = formats
            .formats
            .iter()
            .find(|f| f.type_ == PictType::DIRECT && f.depth == 32 && f.direct.alpha_mask == 0xff && f.direct.alpha_shift == 24 && f.direct.red_shift == 16)
            .map(|f| f.id)
        else {
            return Ok(());
        };
        let pixmap = self.conn.generate_id()?;
        self.conn.create_pixmap(32, pixmap, self.root, w, h)?;
        let gc = self.conn.generate_id()?;
        self.conn.create_gc(gc, pixmap, &CreateGCAux::new())?;
        self.conn.put_image(ImageFormat::Z_PIXMAP, pixmap, gc, w, h, 0, 0, 0, 32, &pixels[..usize::from(w) * usize::from(h) * 4])?;
        let picture = self.conn.generate_id()?;
        self.conn.render_create_picture(picture, pixmap, format, &CreatePictureAux::new())?;
        let cursor = self.conn.generate_id()?;
        self.conn.render_create_cursor(cursor, picture, hotspot.0, hotspot.1)?;
        self.conn.change_window_attributes(self.root, &ChangeWindowAttributesAux::new().cursor(cursor))?;
        self.conn.free_cursor(cursor)?;
        self.conn.render_free_picture(picture)?;
        self.conn.free_gc(gc)?;
        self.conn.free_pixmap(pixmap)?;
        self.conn.flush()?;
        Ok(())
    }

    /// When a resize waiting for an app's redraw (`_NET_WM_SYNC_REQUEST`)
    /// goes anyway, the earliest.
    pub fn sync_deadline(&self) -> Option<std::time::Instant> {
        self.windows.values().filter_map(XWin::sync_deadline).min()
    }

    /// Resizes that waited long enough for an app's redraw go.
    pub fn sync_expired(&self) {
        let now = std::time::Instant::now();
        for win in self.windows.values() {
            if let Err(err) = win.sync_expired(now) {
                tracing::debug!(%err, "cannot resize an X11 window");
            }
        }
    }

    pub fn windows(&self) -> impl Iterator<Item = &XWin> {
        self.windows.values()
    }
}
