//! One X11 window as Helix's window manager knows it: what the client
//! said about it (ICCCM and EWMH properties), where Helix put it, and
//! the Wayland surface Xwayland shows it in.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use smithay::output::{Output, WeakOutput};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Resource};
use smithay::utils::{IsAlive, Logical, Rectangle, Size};
use smithay::xwayland::xwm::WmWindowType;
use smithay::xwayland::XWaylandClientData;
use x11rb::connection::Connection;
use x11rb::errors::ConnectionError;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConfigureNotifyEvent, ConfigureWindowAux, ConnectionExt as _, EventMask,
    InputFocus, PropMode, StackMode, Window as XId, CONFIGURE_NOTIFY_EVENT,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::atoms::Atoms;

/// What a window's `WM_HINTS` and `WM_NORMAL_HINTS` say (ICCCM 4.1.2.3,
/// 4.1.2.4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Hints {
    /// Whether the client wants the X input focus set on it. A window
    /// without `WM_HINTS`, or without the input flag, does (as every
    /// window manager treats it; Chromium and mupdf set none).
    pub input: bool,
    pub urgent: bool,
    /// `WM_HINTS` asks for the window to start iconic.
    pub iconic: bool,
    pub min: Option<(i32, i32)>,
    pub max: Option<(i32, i32)>,
}

#[derive(Debug, Default)]
pub(super) struct State {
    pub override_redirect: bool,
    /// An InputOnly window: shown as asked, never managed.
    pub input_only: bool,
    /// Helix manages it: mapped through a MapRequest and not withdrawn.
    pub managed: bool,
    /// Shown on the X server (mapped).
    pub viewable: bool,
    pub destroyed: bool,
    pub wl_surface: Option<WlSurface>,
    pub serial: Option<u64>,
    pub class: String,
    pub instance: String,
    pub pid: Option<u32>,
    pub hints: Hints,
    pub hints_set: bool,
    pub transient_for: Option<XId>,
    pub delete_window: bool,
    pub take_focus: bool,
    pub window_type: Vec<u32>,
    /// The client draws its own decorations (`_MOTIF_WM_HINTS` asks for
    /// none from the window manager).
    pub client_decorated: bool,
    /// `WM_NAME` and `_NET_WM_NAME`; the title is the second if set.
    pub wm_name: String,
    pub net_name: Option<String>,
    /// `_GTK_FRAME_EXTENTS`: left, right, top, bottom.
    pub frame_extents: [i32; 4],
    pub net_state: HashSet<u32>,
    /// Where Helix last put it, in Helix's logical coordinates.
    pub last_configure: Rectangle<i32, Logical>,
    pub outputs: Vec<(WeakOutput, Rectangle<i32, Logical>)>,
    pub sync: SyncState,
    /// What `_NET_FRAME_EXTENTS` and `_NET_WM_DESKTOP` say now (written
    /// again only when they change).
    pub frame_top: Option<u32>,
    pub desktop: Option<u32>,
}

/// `_NET_WM_SYNC_REQUEST` (EWMH): a client that takes part is told a
/// number before each resize and sets its counter to it once it has drawn
/// the new size; the next resize waits for that, so resizes don't show
/// half-drawn frames. One that doesn't answer in time isn't waited for
/// again.
#[derive(Debug, Default)]
pub(super) struct SyncState {
    /// It lists `_NET_WM_SYNC_REQUEST` in `WM_PROTOCOLS`.
    pub request: bool,
    /// Its `_NET_WM_SYNC_REQUEST_COUNTER`.
    pub counter: Option<u32>,
    /// The Sync alarm on the counter (none yet: 0).
    pub alarm: u32,
    /// The last number it was told.
    pub value: i64,
    /// The size of the last resize it was told about.
    pub size: Option<Size<i32, Logical>>,
    /// Until when the last one is waited for.
    pub wait: Option<Instant>,
    /// The resize waiting for it.
    pub queued: Option<Rectangle<i32, Logical>>,
    /// It didn't answer in time once.
    pub broken: bool,
}

/// How long a resize waits for an app to draw the last one.
pub(super) const SYNC_TIMEOUT: Duration = Duration::from_millis(200);

#[derive(Debug)]
pub(super) struct Shared {
    pub id: XId,
    pub conn: Weak<RustConnection>,
    pub atoms: Atoms,
    pub root: XId,
    pub client: Option<Client>,
    /// The window `_NET_ACTIVE_WINDOW` names (shared by all windows).
    pub active: Arc<AtomicU32>,
    /// The window Helix last gave the X input focus, or the app moved it
    /// to among its own windows (shared by all windows).
    pub given: Arc<AtomicU32>,
    /// The sequence number of the window manager's last SetInputFocus
    /// (focus events from before it are stale).
    pub focus_seq: Arc<AtomicU32>,
    pub state: Mutex<State>,
}

/// An X11 window. Equal only to itself: a window id the X server gives
/// out again later is a new window.
#[derive(Debug, Clone)]
pub struct XWin(pub(super) Arc<Shared>);

impl PartialEq for XWin {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl XWin {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(id: XId, conn: &Arc<RustConnection>, atoms: Atoms, root: XId, client: Option<Client>, focus: (Arc<AtomicU32>, Arc<AtomicU32>, Arc<AtomicU32>), override_redirect: bool, geometry: Rectangle<i32, Logical>) -> Self {
        let state = State { override_redirect, hints: Hints { input: true, ..Hints::default() }, last_configure: geometry, ..State::default() };
        let (active, given, focus_seq) = focus;
        Self(Arc::new(Shared { id, conn: Arc::downgrade(conn), atoms, root, client, active, given, focus_seq, state: Mutex::new(state) }))
    }

    pub(super) fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn window_id(&self) -> XId {
        self.0.id
    }

    pub fn wl_surface(&self) -> Option<WlSurface> {
        self.state().wl_surface.clone()
    }

    pub fn is_override_redirect(&self) -> bool {
        self.state().override_redirect
    }

    pub fn title(&self) -> String {
        let state = self.state();
        state.net_name.clone().unwrap_or_else(|| state.wm_name.clone())
    }

    pub fn class(&self) -> String {
        self.state().class.clone()
    }

    pub fn instance(&self) -> String {
        self.state().instance.clone()
    }

    pub fn pid(&self) -> Option<u32> {
        self.state().pid
    }

    pub fn hints(&self) -> Hints {
        self.state().hints
    }

    pub fn transient_for(&self) -> Option<XId> {
        self.state().transient_for
    }

    /// Whether the client draws its own decorations (as Smithay's
    /// `is_decorated` means it).
    pub fn is_decorated(&self) -> bool {
        self.state().client_decorated
    }

    /// The window's type: the first of its `_NET_WM_WINDOW_TYPE` that is
    /// known (EWMH lists them in order of preference).
    pub fn window_type(&self) -> Option<WmWindowType> {
        let a = &self.0.atoms;
        let types = self.state().window_type.clone();
        types.into_iter().find_map(|t| {
            Some(match t {
                t if t == a._NET_WM_WINDOW_TYPE_COMBO => WmWindowType::Combo,
                t if t == a._NET_WM_WINDOW_TYPE_DESKTOP => WmWindowType::Desktop,
                t if t == a._NET_WM_WINDOW_TYPE_DND => WmWindowType::Dnd,
                t if t == a._NET_WM_WINDOW_TYPE_DOCK => WmWindowType::Dock,
                t if t == a._NET_WM_WINDOW_TYPE_DROPDOWN_MENU => WmWindowType::DropdownMenu,
                t if t == a._NET_WM_WINDOW_TYPE_DIALOG => WmWindowType::Dialog,
                t if t == a._NET_WM_WINDOW_TYPE_MENU => WmWindowType::Menu,
                t if t == a._NET_WM_WINDOW_TYPE_NOTIFICATION => WmWindowType::Notification,
                t if t == a._NET_WM_WINDOW_TYPE_NORMAL => WmWindowType::Normal,
                t if t == a._NET_WM_WINDOW_TYPE_POPUP_MENU => WmWindowType::PopupMenu,
                t if t == a._NET_WM_WINDOW_TYPE_SPLASH => WmWindowType::Splash,
                t if t == a._NET_WM_WINDOW_TYPE_TOOLBAR => WmWindowType::Toolbar,
                t if t == a._NET_WM_WINDOW_TYPE_TOOLTIP => WmWindowType::Tooltip,
                t if t == a._NET_WM_WINDOW_TYPE_UTILITY => WmWindowType::Utility,
                _ => return None,
            })
        })
    }

    pub fn min_size(&self) -> Option<Size<i32, Logical>> {
        self.state().hints.min.map(|(w, h)| self.size_from_x(w, h))
    }

    pub fn max_size(&self) -> Option<Size<i32, Logical>> {
        self.state().hints.max.map(|(w, h)| self.size_from_x(w, h))
    }

    fn has(&self, atom: u32) -> bool {
        self.state().net_state.contains(&atom)
    }

    pub fn is_modal(&self) -> bool {
        self.has(self.0.atoms._NET_WM_STATE_MODAL)
    }

    pub fn is_above(&self) -> bool {
        self.has(self.0.atoms._NET_WM_STATE_ABOVE)
    }

    pub fn is_skip_taskbar(&self) -> bool {
        self.has(self.0.atoms._NET_WM_STATE_SKIP_TASKBAR)
    }

    pub fn is_fullscreen(&self) -> bool {
        self.has(self.0.atoms._NET_WM_STATE_FULLSCREEN)
    }

    /// Maximized both ways (EWMH apps set both).
    pub fn is_maximized(&self) -> bool {
        self.has(self.0.atoms._NET_WM_STATE_MAXIMIZED_HORZ) && self.has(self.0.atoms._NET_WM_STATE_MAXIMIZED_VERT)
    }

    pub fn is_activated(&self) -> bool {
        self.has(self.0.atoms._NET_WM_STATE_FOCUSED)
    }

    pub fn last_configure(&self) -> Rectangle<i32, Logical> {
        self.state().last_configure
    }

    /// The window without the shadow GTK draws around it
    /// (`_GTK_FRAME_EXTENTS`), relative to its own origin.
    pub fn geometry(&self) -> Rectangle<i32, Logical> {
        let bbox = self.bbox();
        let [l, r, t, b] = self.state().frame_extents;
        let (l, r, t, b) = (self.x_to_logical(l), self.x_to_logical(r), self.x_to_logical(t), self.x_to_logical(b));
        Rectangle::new((l, t).into(), ((bbox.size.w - l - r).max(0), (bbox.size.h - t - b).max(0)).into())
    }

    /// The shadow GTK draws around it (`_GTK_FRAME_EXTENTS`), in Helix's
    /// logical coordinates: left, right, top, bottom.
    pub fn shadow(&self) -> [i32; 4] {
        let e = self.state().frame_extents;
        e.map(|v| self.x_to_logical(v))
    }

    /// The window's own rectangle: its surface's size once it has one.
    pub fn bbox(&self) -> Rectangle<i32, Logical> {
        let state = self.state();
        let size = state
            .wl_surface
            .as_ref()
            .and_then(|s| {
                smithay::wayland::compositor::with_states(s, |states| {
                    states.data_map.get::<smithay::backend::renderer::utils::RendererSurfaceStateUserData>().and_then(|d| d.lock().unwrap().surface_size())
                })
            })
            .unwrap_or(state.last_configure.size);
        Rectangle::from_size(size)
    }

    pub(super) fn conn(&self) -> Option<Arc<RustConnection>> {
        self.0.conn.upgrade()
    }

    /// Xwayland's scale: X11 pixels per Helix logical pixel.
    fn scale(&self) -> f64 {
        client_scale(self.0.client.as_ref())
    }

    pub(super) fn x_to_logical(&self, v: i32) -> i32 {
        (f64::from(v) / self.scale()).round() as i32
    }

    pub(super) fn logical_to_x(&self, v: i32) -> i32 {
        (f64::from(v) * self.scale()).round() as i32
    }

    fn size_from_x(&self, w: i32, h: i32) -> Size<i32, Logical> {
        (self.x_to_logical(w), self.x_to_logical(h)).into()
    }

    /// Moves and resizes it (`rect`, or where it already is), and tells
    /// the client where it is now, in root coordinates (ICCCM 4.1.5: a
    /// synthetic ConfigureNotify, also when nothing changed, so a client
    /// waiting for an answer gets one).
    pub fn configure(&self, rect: Option<Rectangle<i32, Logical>>) -> Result<(), ConnectionError> {
        let Some(conn) = self.conn() else { return Err(ConnectionError::UnknownError) };
        let rect = {
            let mut state = self.state();
            if state.override_redirect {
                return Ok(());
            }
            let rect = rect.unwrap_or(state.last_configure);
            state.last_configure = rect;
            rect
        };
        if !self.sync_before(&conn, rect)? {
            return Ok(());
        }
        let (x, y, w, h) = (self.logical_to_x(rect.loc.x), self.logical_to_x(rect.loc.y), self.logical_to_x(rect.size.w).max(1), self.logical_to_x(rect.size.h).max(1));
        conn.configure_window(self.0.id, &ConfigureWindowAux::new().x(x).y(y).width(w as u32).height(h as u32).border_width(0))?;
        let event = ConfigureNotifyEvent {
            response_type: CONFIGURE_NOTIFY_EVENT,
            sequence: 0,
            event: self.0.id,
            window: self.0.id,
            above_sibling: x11rb::NONE,
            x: x as i16,
            y: y as i16,
            width: w as u16,
            height: h as u16,
            border_width: 0,
            override_redirect: false,
        };
        conn.send_event(false, self.0.id, EventMask::STRUCTURE_NOTIFY, event)?;
        conn.flush()
    }

    /// Before giving it `rect`: if it syncs (`_NET_WM_SYNC_REQUEST`) and
    /// this is a resize, tell it the next number and watch its counter
    /// for it; while it hasn't drawn the last resize, `rect` waits
    /// (false: not now).
    fn sync_before(&self, conn: &RustConnection, rect: Rectangle<i32, Logical>) -> Result<bool, ConnectionError> {
        use x11rb::protocol::sync::{ChangeAlarmAux, ConnectionExt as _, CreateAlarmAux, Int64, TESTTYPE, VALUETYPE};
        let mut state = self.state();
        // Not before it is shown and has a surface to draw in.
        let shown = state.wl_surface.is_some();
        let sync = &mut state.sync;
        let Some(counter) = sync.counter.filter(|_| sync.request && !sync.broken && shown) else { return Ok(true) };
        let resize = sync.size != Some(rect.size);
        if !resize && sync.queued.is_none() {
            return Ok(true);
        }
        if let Some(until) = sync.wait {
            if Instant::now() < until {
                sync.queued = Some(rect);
                return Ok(false);
            }
            // It never drew the last one: no more waiting for it.
            sync.broken = true;
            sync.wait = None;
            sync.queued = None;
            return Ok(true);
        }
        sync.queued = None;
        if !resize {
            return Ok(true);
        }
        sync.value += 1;
        let value = Int64 { hi: (sync.value >> 32) as i32, lo: sync.value as u32 };
        if sync.alarm == 0 {
            let Ok(alarm) = conn.generate_id() else { return Ok(true) };
            let aux = CreateAlarmAux::new().counter(counter).value_type(VALUETYPE::ABSOLUTE).value(value).test_type(TESTTYPE::POSITIVE_COMPARISON).delta(Int64 { hi: 0, lo: 0 }).events(1);
            conn.sync_create_alarm(alarm, &aux)?;
            sync.alarm = alarm;
        } else {
            conn.sync_change_alarm(sync.alarm, &ChangeAlarmAux::new().value(value).events(1))?;
        }
        let message = ClientMessageEvent::new(32, self.0.id, self.0.atoms.WM_PROTOCOLS, [self.0.atoms._NET_WM_SYNC_REQUEST, x11rb::CURRENT_TIME, value.lo, value.hi as u32, 0]);
        conn.send_event(false, self.0.id, EventMask::NO_EVENT, message)?;
        sync.size = Some(rect.size);
        sync.wait = Some(Instant::now() + SYNC_TIMEOUT);
        Ok(true)
    }

    /// Its sync alarm went off: it drew the last resize; the one waiting
    /// goes now.
    pub(super) fn sync_done(&self) -> Result<(), ConnectionError> {
        let queued = {
            let mut state = self.state();
            state.sync.wait = None;
            state.sync.queued.take()
        };
        match queued {
            Some(rect) => self.configure(Some(rect)),
            None => Ok(()),
        }
    }

    /// When the resize waiting for it goes anyway, if one does.
    pub(super) fn sync_deadline(&self) -> Option<Instant> {
        let state = self.state();
        state.sync.queued.and(state.sync.wait)
    }

    /// The resize waiting for it goes if it waited long enough (and the
    /// app isn't waited for again).
    pub(super) fn sync_expired(&self, now: Instant) -> Result<(), ConnectionError> {
        if self.sync_deadline().is_some_and(|d| d <= now) {
            // Past its deadline: configure doesn't wait any more.
            self.configure(None)
        } else {
            Ok(())
        }
    }

    /// Sets or clears EWMH states in `_NET_WM_STATE`.
    pub(super) fn change_state(&self, add: &[u32], remove: &[u32]) -> Result<(), ConnectionError> {
        let atoms: Vec<u32> = {
            let mut state = self.state();
            // Nothing to change: the property stays as it is (clients
            // watching it see no needless PropertyNotify).
            if add.iter().all(|a| state.net_state.contains(a)) && remove.iter().all(|a| !state.net_state.contains(a)) {
                return Ok(());
            }
            for a in remove {
                state.net_state.remove(a);
            }
            for a in add {
                state.net_state.insert(*a);
            }
            if !state.managed {
                return Ok(());
            }
            let mut v: Vec<u32> = state.net_state.iter().copied().collect();
            v.sort_unstable();
            v
        };
        let Some(conn) = self.conn() else { return Err(ConnectionError::UnknownError) };
        conn.change_property32(PropMode::REPLACE, self.0.id, self.0.atoms._NET_WM_STATE, AtomEnum::ATOM, &atoms)?;
        conn.flush()
    }

    fn set_state(&self, atom: u32, on: bool) -> Result<(), ConnectionError> {
        if on {
            self.change_state(&[atom], &[])
        } else {
            self.change_state(&[], &[atom])
        }
    }

    pub fn set_fullscreen(&self, on: bool) -> Result<(), ConnectionError> {
        self.set_state(self.0.atoms._NET_WM_STATE_FULLSCREEN, on)
    }

    pub fn set_maximized(&self, on: bool) -> Result<(), ConnectionError> {
        let a = &self.0.atoms;
        if on {
            self.change_state(&[a._NET_WM_STATE_MAXIMIZED_HORZ, a._NET_WM_STATE_MAXIMIZED_VERT], &[])
        } else {
            self.change_state(&[], &[a._NET_WM_STATE_MAXIMIZED_HORZ, a._NET_WM_STATE_MAXIMIZED_VERT])
        }
    }

    pub fn set_above(&self, on: bool) -> Result<(), ConnectionError> {
        self.set_state(self.0.atoms._NET_WM_STATE_ABOVE, on)
    }

    pub fn set_modal(&self, on: bool) -> Result<(), ConnectionError> {
        self.set_state(self.0.atoms._NET_WM_STATE_MODAL, on)
    }

    /// Minimized or shown again: `WM_STATE` Iconic or Normal (ICCCM
    /// 4.1.4) and `_NET_WM_STATE_HIDDEN` (EWMH).
    pub fn set_hidden(&self, on: bool) -> Result<(), ConnectionError> {
        if self.is_override_redirect() {
            return Ok(());
        }
        self.set_state(self.0.atoms._NET_WM_STATE_HIDDEN, on)?;
        let Some(conn) = self.conn() else { return Err(ConnectionError::UnknownError) };
        let state = if on { 3 } else { 1 };
        conn.change_property32(PropMode::REPLACE, self.0.id, self.0.atoms.WM_STATE, self.0.atoms.WM_STATE, &[state, x11rb::NONE])?;
        conn.flush()
    }

    /// Marks it active or not (`_NET_WM_STATE_FOCUSED`, and the root's
    /// `_NET_ACTIVE_WINDOW`).
    pub fn set_activated(&self, on: bool) -> Result<(), ConnectionError> {
        if self.is_override_redirect() {
            return Ok(());
        }
        self.set_state(self.0.atoms._NET_WM_STATE_FOCUSED, on)?;
        let Some(conn) = self.conn() else { return Err(ConnectionError::UnknownError) };
        if on {
            if self.0.active.swap(self.0.id, Ordering::Relaxed) == self.0.id {
                return conn.flush();
            }
            conn.change_property32(PropMode::REPLACE, self.0.root, self.0.atoms._NET_ACTIVE_WINDOW, AtomEnum::WINDOW, &[self.0.id])?;
        } else if self.0.active.compare_exchange(self.0.id, x11rb::NONE, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
            // Only while it still names this window.
            conn.change_property32(PropMode::REPLACE, self.0.root, self.0.atoms._NET_ACTIVE_WINDOW, AtomEnum::WINDOW, &[x11rb::NONE])?;
        }
        conn.flush()
    }

    /// Gives it the X input focus, as its input model asks (ICCCM 4.1.7):
    /// set directly when it takes input, and `WM_TAKE_FOCUS` when it
    /// asks for that.
    pub(super) fn focus(&self) -> Result<(), ConnectionError> {
        let (input, take_focus) = {
            let state = self.state();
            (state.hints.input, state.take_focus)
        };
        let Some(conn) = self.conn() else { return Err(ConnectionError::UnknownError) };
        // Globally active windows (no input flag, WM_TAKE_FOCUS) set the
        // focus themselves; their FocusIn counts as given too.
        self.0.given.store(self.0.id, Ordering::Relaxed);
        if input {
            let cookie = conn.set_input_focus(InputFocus::POINTER_ROOT, self.0.id, x11rb::CURRENT_TIME)?;
            self.0.focus_seq.store(cookie.sequence_number() as u32, Ordering::Relaxed);
        }
        if take_focus {
            let event = ClientMessageEvent::new(32, self.0.id, self.0.atoms.WM_PROTOCOLS, [self.0.atoms.WM_TAKE_FOCUS, x11rb::CURRENT_TIME, 0, 0, 0]);
            conn.send_event(false, self.0.id, EventMask::NO_EVENT, event)?;
        }
        conn.flush()
    }

    /// Asks it to close: `WM_DELETE_WINDOW` when it takes part, otherwise
    /// the client is disconnected (`KillClient`), as ICCCM has it.
    pub fn close(&self) -> Result<(), ConnectionError> {
        let Some(conn) = self.conn() else { return Err(ConnectionError::UnknownError) };
        if self.state().delete_window {
            let event = ClientMessageEvent::new(32, self.0.id, self.0.atoms.WM_PROTOCOLS, [self.0.atoms.WM_DELETE_WINDOW, x11rb::CURRENT_TIME, 0, 0, 0]);
            conn.send_event(false, self.0.id, EventMask::NO_EVENT, event)?;
        } else {
            conn.kill_client(self.0.id)?;
        }
        conn.flush()
    }

    /// Puts it on top of the X stack.
    pub fn raise(&self) -> Result<(), ConnectionError> {
        let Some(conn) = self.conn() else { return Err(ConnectionError::UnknownError) };
        conn.configure_window(self.0.id, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))?;
        conn.flush()
    }

    /// Says how much of it is the title bar Helix draws above it
    /// (`_NET_FRAME_EXTENTS`: left, right, top, bottom).
    pub fn set_frame_extents(&self, top: i32) -> Result<(), ConnectionError> {
        let Some(conn) = self.conn() else { return Err(ConnectionError::UnknownError) };
        let top = self.logical_to_x(top).max(0) as u32;
        if self.state().frame_top.replace(top) == Some(top) {
            return Ok(());
        }
        conn.change_property32(PropMode::REPLACE, self.0.id, self.0.atoms._NET_FRAME_EXTENTS, AtomEnum::CARDINAL, &[0, 0, top, 0])?;
        conn.flush()
    }

    /// Says which workspace it is on (`_NET_WM_DESKTOP`; `u32::MAX`: on
    /// all of them).
    pub fn set_desktop(&self, desktop: u32) -> Result<(), ConnectionError> {
        let Some(conn) = self.conn() else { return Err(ConnectionError::UnknownError) };
        {
            let mut state = self.state();
            if !state.managed || state.desktop.replace(desktop) == Some(desktop) {
                return Ok(());
            }
        }
        conn.change_property32(PropMode::REPLACE, self.0.id, self.0.atoms._NET_WM_DESKTOP, AtomEnum::CARDINAL, &[desktop])?;
        conn.flush()
    }

    pub(crate) fn output_enter(&self, output: &Output, overlap: Rectangle<i32, Logical>) {
        let mut state = self.state();
        state.outputs.retain(|(o, _)| o.is_alive() && o != output);
        state.outputs.push((output.downgrade(), overlap));
        drop(state);
        self.refresh();
    }

    pub(crate) fn output_leave(&self, output: &Output) {
        let surface = {
            let mut state = self.state();
            state.outputs.retain(|(o, _)| o != output);
            state.wl_surface.clone()
        };
        if let Some(surface) = surface {
            smithay::desktop::utils::output_update(output, None, &surface);
        }
    }

    pub(crate) fn refresh(&self) {
        let (surface, outputs) = {
            let state = self.state();
            (state.wl_surface.clone(), state.outputs.clone())
        };
        let Some(surface) = surface else { return };
        for (weak, overlap) in outputs {
            if let Some(output) = weak.upgrade() {
                smithay::desktop::utils::output_update(&output, Some(overlap), &surface);
            }
        }
    }
}

impl IsAlive for XWin {
    fn alive(&self) -> bool {
        !self.state().destroyed && self.0.conn.strong_count() > 0
    }
}

/// Xwayland's scale (X11 pixels per Helix logical pixel) for its client.
pub(super) fn client_scale(client: Option<&Client>) -> f64 {
    client.and_then(|c| c.get_data::<XWaylandClientData>()).map_or(1.0, |d| d.compositor_state.client_scale()).max(1.0)
}

/// Whether `surface` came from the Xwayland client this window manager
/// serves.
pub(super) fn from_client(surface: &WlSurface, client: &Client) -> bool {
    surface.client().is_some_and(|c| c.id() == client.id())
}
