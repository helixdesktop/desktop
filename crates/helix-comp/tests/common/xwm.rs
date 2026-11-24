//! Helpers for the X11 window manager's edge cases (`tests/x11_cases.md`,
//! #191): an X11 client of the session's Xwayland, and a stand-in shell
//! that sees Helix's window list and acts on windows through it.
//!
//! They look only at what an X11 app or the shell sees (properties,
//! events, focus, the window list), never at how the window manager does
//! it, so the same tests hold for Smithay's window manager and Helix's.

use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use helix_protocols::client::window_list::helix_window_list_v1::{self, HelixWindowListV1};
use helix_test_support::headless::{HeadlessSession, Options};
use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection as WlConnection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::session_lock::v1::client::{ext_session_lock_manager_v1, ext_session_lock_surface_v1, ext_session_lock_v1};
use wayland_protocols::xdg::activation::v1::client::{xdg_activation_token_v1, xdg_activation_v1};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConnectionExt, CreateWindowAux, EventMask, PropMode, UnmapNotifyEvent, Window, WindowClass, UNMAP_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// The screen every session here has.
pub const SCREEN: (u16, u16) = (800, 600);

/// A headless session with Xwayland, or `None` (skipped) without one.
pub fn session() -> Option<HeadlessSession> {
    session_env(Vec::new())
}

/// [`session`] with `env` for the compositor (e.g. two monitors side by
/// side: `HELIX_HEADLESS_OUTPUTS=2`).
pub fn session_env(env: Vec<(String, String)>) -> Option<HeadlessSession> {
    session_settings_env("", env)
}

/// [`session`] with `settings` (`GSETTINGS_BACKEND=keyfile` contents:
/// `[org/gnome/…]` groups) and `env` for the compositor.
pub fn session_settings_env(settings: &str, env: Vec<(String, String)>) -> Option<HeadlessSession> {
    let comp = PathBuf::from(env!("CARGO_BIN_EXE_helix-comp"));
    let session = HeadlessSession::start(Options { comp: &comp, shell: None, size: (u32::from(SCREEN.0), u32::from(SCREEN.1)), settings, env })?;
    if super::no_xwayland(&session) {
        return None;
    }
    Some(session)
}

/// A session whose shell is the test: it sees the window list and acts on
/// windows through it.
pub fn session_with_shell(name: &str) -> Option<(HeadlessSession, Shell)> {
    session_with_shell_env(name, Vec::new())
}

/// [`session_with_shell`] with `env` for the compositor (e.g. two monitors:
/// `HELIX_HEADLESS_OUTPUTS=2`).
pub fn session_with_shell_env(name: &str, env: Vec<(String, String)>) -> Option<(HeadlessSession, Shell)> {
    session_with_shell_settings(name, env, "")
}

/// [`session_with_shell_env`] with `settings` (a GSettings keyfile, e.g.
/// `[org/gnome/mutter]\ncheck-alive-timeout=uint32 1000\n`).
pub fn session_with_shell_settings(name: &str, env: Vec<(String, String)>, settings: &str) -> Option<(HeadlessSession, Shell)> {
    let comp = PathBuf::from(env!("CARGO_BIN_EXE_helix-comp"));
    let dir = std::env::temp_dir().join(format!("helix-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (program, listener) = stand_in_shell(&dir);
    let session = HeadlessSession::start(Options { comp: &comp, shell: Some(&program), size: (u32::from(SCREEN.0), u32::from(SCREEN.1)), settings, env })?;
    if super::no_xwayland(&session) {
        return None;
    }
    let shell = Shell::new(shell_socket(&listener), dir);
    Some((session, shell))
}

/// Whether the `Xwayland` on `PATH` is at least `major.minor` (from
/// `Xwayland -version`); unknown counts as no.
///
/// With `HELIX_REQUIRE_NEW_XWAYLAND=1` (the CI job on a newer Ubuntu, #237)
/// an older one is a failure, not a skip: that job is there to run them.
pub fn xwayland_at_least(major: u32, minor: u32) -> bool {
    let new_enough = (|| {
        let out = std::process::Command::new("Xwayland").arg("-version").output().ok()?;
        let text = String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout);
        let version = text.split("Xwayland Version ").nth(1).and_then(|v| v.split_whitespace().next())?.to_owned();
        let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
        Some((parts.next().unwrap_or(0), parts.next().unwrap_or(0)) >= (major, minor))
    })()
    .unwrap_or(false);
    assert!(new_enough || std::env::var("HELIX_REQUIRE_NEW_XWAYLAND").map_or(true, |v| v != "1"), "Xwayland older than {major}.{minor}, and HELIX_REQUIRE_NEW_XWAYLAND=1");
    new_enough
}

/// Wait until `done` holds (polling), or fail with `what`.
pub fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// An X11 client of the session's Xwayland.
pub struct X {
    pub conn: RustConnection,
    pub root: Window,
}

impl X {
    pub fn new(session: &HeadlessSession) -> Self {
        let (conn, screen_num) = super::x11(session);
        let root = conn.setup().roots[screen_num].root;
        X { conn, root }
    }

    pub fn atom(&self, name: &str) -> u32 {
        self.conn.intern_atom(false, name.as_bytes()).unwrap().reply().unwrap().atom
    }

    pub fn flush(&self) {
        self.conn.flush().unwrap();
    }

    /// An InputOutput top-level window at (`x`, `y`), not mapped, whose
    /// structure and property changes this client hears.
    pub fn create(&self, (x, y): (i16, i16), (w, h): (u16, u16)) -> Window {
        self.create_with(x, y, w, h, CreateWindowAux::new())
    }

    /// [`Self::create`] with more attributes.
    pub fn create_with(&self, x: i16, y: i16, w: u16, h: u16, aux: CreateWindowAux) -> Window {
        let screen = &self.conn.setup().roots[0];
        let window = self.conn.generate_id().unwrap();
        let aux = aux.background_pixel(screen.white_pixel).event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE | EventMask::FOCUS_CHANGE);
        self.conn.create_window(0, window, self.root, x, y, w, h, 0, WindowClass::INPUT_OUTPUT, 0, &aux).unwrap();
        self.flush();
        window
    }

    /// A plain 200×100 window, not mapped.
    pub fn window(&self) -> Window {
        self.create((10, 10), (200, 100))
    }

    /// An override-redirect window (a menu, a tooltip), mapped.
    pub fn popup(&self, (x, y): (i16, i16), (w, h): (u16, u16)) -> Window {
        let window = self.create_with(x, y, w, h, CreateWindowAux::new().override_redirect(1));
        self.conn.map_window(window).unwrap();
        self.flush();
        window
    }

    pub fn set32(&self, window: Window, property: &str, kind: impl Into<u32>, data: &[u32]) {
        let property = self.atom(property);
        self.conn.change_property32(PropMode::REPLACE, window, property, kind.into(), data).unwrap();
        self.flush();
    }

    pub fn set8(&self, window: Window, property: &str, kind: impl Into<u32>, data: &[u8]) {
        let property = self.atom(property);
        self.conn.change_property8(PropMode::REPLACE, window, property, kind.into(), data).unwrap();
        self.flush();
    }

    pub fn set_atoms(&self, window: Window, property: &str, atoms: &[&str]) {
        let atoms: Vec<u32> = atoms.iter().map(|a| self.atom(a)).collect();
        self.set32(window, property, AtomEnum::ATOM, &atoms);
    }

    pub fn delete(&self, window: Window, property: &str) {
        let property = self.atom(property);
        self.conn.delete_property(window, property).unwrap();
        self.flush();
    }

    /// A property's 32-bit values, `None` when it is not set.
    pub fn get32(&self, window: Window, property: &str) -> Option<Vec<u32>> {
        let property = self.atom(property);
        let reply = self.conn.get_property(false, window, property, AtomEnum::ANY, 0, 1 << 16).unwrap().reply().ok()?;
        if reply.type_ == x11rb::NONE {
            return None;
        }
        reply.value32().map(|v| v.collect())
    }

    /// A property's bytes and type, `None` when it is not set.
    pub fn get8(&self, window: Window, property: &str) -> Option<(u32, Vec<u8>)> {
        let property = self.atom(property);
        let reply = self.conn.get_property(false, window, property, AtomEnum::ANY, 0, 1 << 16).unwrap().reply().ok()?;
        (reply.type_ != x11rb::NONE).then_some((reply.type_, reply.value))
    }

    /// The window's `WM_STATE` (1 Normal, 3 Iconic, 0 Withdrawn), if set.
    pub fn wm_state(&self, window: Window) -> Option<u32> {
        self.get32(window, "WM_STATE").and_then(|v| v.first().copied())
    }

    /// Whether the window manager has taken `window` in (`WM_STATE` Normal).
    pub fn managed(&self, window: Window) -> bool {
        self.wm_state(window) == Some(1)
    }

    pub fn map(&self, window: Window) {
        self.conn.map_window(window).unwrap();
        self.flush();
    }

    /// Map `window` and wait until the window manager manages it.
    pub fn map_managed(&self, window: Window) {
        self.map(window);
        until("the window manager never managed the window", || self.managed(window));
    }

    /// A plain window, mapped and managed.
    pub fn mapped(&self) -> Window {
        let window = self.window();
        self.map_managed(window);
        window
    }

    /// A window titled `title`, mapped and managed.
    pub fn mapped_titled(&self, title: &str) -> Window {
        let window = self.window();
        self.set8(window, "_NET_WM_NAME", self.atom("UTF8_STRING"), title.as_bytes());
        self.map_managed(window);
        window
    }

    /// Hide `window` as `XWithdrawWindow` does: unmap it and tell the
    /// window manager with a synthetic UnmapNotify (ICCCM 4.1.4).
    pub fn withdraw(&self, window: Window) {
        self.conn.unmap_window(window).unwrap();
        self.synthetic_unmap(window);
    }

    /// The synthetic UnmapNotify half of a withdrawal.
    pub fn synthetic_unmap(&self, window: Window) {
        let notify = UnmapNotifyEvent { response_type: UNMAP_NOTIFY_EVENT, sequence: 0, event: self.root, window, from_configure: false };
        self.conn.send_event(false, self.root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, notify).unwrap();
        self.flush();
    }

    /// Send the window manager a client message about `window` (to the
    /// root, as EWMH asks).
    pub fn tell_wm(&self, window: Window, kind: &str, data: [u32; 5]) {
        let message = ClientMessageEvent::new(32, window, self.atom(kind), data);
        self.conn.send_event(false, self.root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, message).unwrap();
        self.flush();
    }

    /// Ask to add (1), remove (0) or toggle (2) `_NET_WM_STATE` `states`.
    pub fn net_wm_state(&self, window: Window, action: u32, states: &[&str]) {
        let a = self.atom(states[0]);
        let b = states.get(1).map_or(0, |s| self.atom(s));
        self.tell_wm(window, "_NET_WM_STATE", [action, a, b, 1, 0]);
    }

    /// The window's `_NET_WM_STATE` atoms.
    pub fn net_states(&self, window: Window) -> Vec<u32> {
        self.get32(window, "_NET_WM_STATE").unwrap_or_default()
    }

    pub fn has_state(&self, window: Window, state: &str) -> bool {
        self.net_states(window).contains(&self.atom(state))
    }

    /// The root's `_NET_ACTIVE_WINDOW`.
    pub fn active(&self) -> Option<Window> {
        self.get32(self.root, "_NET_ACTIVE_WINDOW").and_then(|v| v.first().copied())
    }

    /// Ask for `window` to be activated, from `source` (1 an app, 2 a
    /// pager), the app's active window being `current`.
    pub fn activate(&self, window: Window, source: u32, current: Window) {
        self.tell_wm(window, "_NET_ACTIVE_WINDOW", [source, x11rb::CURRENT_TIME, current, 0, 0]);
    }

    /// Activate `window` as a pager does, and wait until it is active.
    /// Waits until the window manager has handled every X event this
    /// client's requests so far caused, and acted on them (#235): it
    /// handles events in order, and answers `_NET_REQUEST_FRAME_EXTENTS`
    /// (for a window it hasn't shown) only once it got there. Not for
    /// what comes through the compositor (pointer input): those events
    /// aren't ordered with this client's. Smithay's window manager doesn't
    /// answer the request: a fixed wait there.
    pub fn sync(&self) {
        if super::xwm_running() != super::Xwm::Helix {
            std::thread::sleep(Duration::from_millis(300));
            return;
        }
        let probe = self.conn.generate_id().unwrap();
        self.conn.create_window(x11rb::COPY_DEPTH_FROM_PARENT, probe, self.root, -10, -10, 1, 1, 0, WindowClass::INPUT_OUTPUT, 0, &CreateWindowAux::new()).unwrap();
        let request = self.atom("_NET_REQUEST_FRAME_EXTENTS");
        let message = ClientMessageEvent::new(32, probe, request, [0, 0, 0, 0, 0]);
        self.conn.send_event(false, self.root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, message).unwrap();
        self.flush();
        let extents = self.atom("_NET_FRAME_EXTENTS");
        until("the window manager never got to a frame-extents request (sync)", || {
            self.conn.get_property(false, probe, extents, AtomEnum::CARDINAL, 0, 4).unwrap().reply().is_ok_and(|r| r.value_len == 4)
        });
        self.conn.destroy_window(probe).unwrap();
        self.flush();
    }

    pub fn focus(&self, window: Window) {
        self.activate(window, 2, x11rb::NONE);
        until("the window never became active", || self.active() == Some(window));
        // `_NET_ACTIVE_WINDOW` is written a moment before the window
        // manager sets the X input focus: wait for that too (a window that
        // takes no input never gets it), or a focus change the test makes
        // next can land first and be undone (#269).
        let end = Instant::now() + Duration::from_secs(2);
        while self.input_focus() != window && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn client_list(&self) -> Vec<Window> {
        self.get32(self.root, "_NET_CLIENT_LIST").unwrap_or_default()
    }

    pub fn stacking_list(&self) -> Vec<Window> {
        self.get32(self.root, "_NET_CLIENT_LIST_STACKING").unwrap_or_default()
    }

    /// The X input focus.
    pub fn input_focus(&self) -> Window {
        self.conn.get_input_focus().unwrap().reply().unwrap().focus
    }

    /// Where the window is on the root.
    pub fn position(&self, window: Window) -> (i16, i16) {
        let t = self.conn.translate_coordinates(window, self.root, 0, 0).unwrap().reply().unwrap();
        (t.dst_x, t.dst_y)
    }

    pub fn size(&self, window: Window) -> (u16, u16) {
        let g = self.conn.get_geometry(window).unwrap().reply().unwrap();
        (g.width, g.height)
    }

    /// The client windows among the root's children, bottom to top. A
    /// child that is a frame the window manager made counts as the client
    /// window inside it, so this is the stacking order the apps see.
    pub fn stacking_order(&self, of: &[Window]) -> Vec<Window> {
        let tree = self.conn.query_tree(self.root).unwrap().reply().unwrap();
        tree.children
            .iter()
            .filter_map(|&child| {
                if of.contains(&child) {
                    return Some(child);
                }
                let inner = self.conn.query_tree(child).ok()?.reply().ok()?;
                inner.children.into_iter().find(|c| of.contains(c))
            })
            .collect()
    }

    /// Drop the events this client got so far.
    pub fn drain(&self) {
        while self.conn.poll_for_event().unwrap().is_some() {}
    }

    /// The events this client gets in the next `time`.
    pub fn events_for(&self, time: Duration) -> Vec<Event> {
        let mut events = Vec::new();
        let end = Instant::now() + time;
        while Instant::now() < end {
            match self.conn.poll_for_event().unwrap() {
                Some(event) => events.push(event),
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        events
    }

    /// The next event `pick` takes, within 5 seconds, or fail with `what`.
    pub fn wait_event<T>(&self, what: &str, mut pick: impl FnMut(&Event) -> Option<T>) -> T {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < end, "{what}");
            match self.conn.poll_for_event().unwrap() {
                Some(event) => {
                    if let Some(found) = pick(&event) {
                        return found;
                    }
                }
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    /// Whether the X server still answers this client promptly (nothing
    /// holds a server grab).
    pub fn answers(&self) -> bool {
        let start = Instant::now();
        let ok = self.conn.get_input_focus().ok().and_then(|c| c.reply().ok()).is_some();
        ok && start.elapsed() < Duration::from_secs(1)
    }
}

/// The session's Xwayland process (`Xwayland :N ...`).
pub fn xwayland_pid(session: &HeadlessSession) -> Option<i32> {
    std::fs::read_dir("/proc").ok()?.flatten().find_map(|entry| {
        let pid: i32 = entry.file_name().to_str()?.parse().ok()?;
        let cmdline = std::fs::read(entry.path().join("cmdline")).ok()?;
        let mut args = cmdline.split(|b| *b == 0);
        let program = args.next()?;
        let ours = program.ends_with(b"Xwayland") && args.next() == Some(session.x_display.as_bytes());
        ours.then_some(pid)
    })
}

/// Kill the session's Xwayland (as a crash would); returns its pid.
pub fn kill_xwayland(session: &HeadlessSession) -> i32 {
    let pid = xwayland_pid(session).expect("the session's Xwayland");
    // SAFETY: signalling the Xwayland of this test's own session.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    pid
}

/// Wait until Xwayland `killed` was replaced by a new one that takes
/// clients, and return a client of it.
pub fn restarted_xwayland(session: &HeadlessSession, killed: i32) -> X {
    let deadline = Instant::now() + Duration::from_secs(10);
    while xwayland_pid(session).is_none_or(|pid| pid == killed) {
        assert!(Instant::now() < deadline, "Xwayland was not started again on {}", session.x_display);
        std::thread::sleep(Duration::from_millis(50));
    }
    loop {
        if let Ok(x) = std::panic::catch_unwind(|| X::new(session)) {
            return x;
        }
        assert!(Instant::now() < deadline, "cannot connect to the new Xwayland");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The window manager still works: a new window from a new client gets
/// managed.
pub fn wm_still_works(session: &HeadlessSession) {
    let x = X::new(session);
    x.mapped();
}

// ---- the stand-in shell ----

/// A window as the window list shows it.
#[derive(Debug, Clone, Default)]
pub struct Listed {
    pub id: u32,
    pub app_id: String,
    pub title: String,
    /// x, y, width, height.
    pub geometry: Option<(i32, i32, i32, i32)>,
    pub minimized: bool,
    /// The size of the window's own icon image (0: none).
    pub icon_size: u32,
}

#[derive(Default)]
pub struct ShellState {
    pub windows: Vec<Listed>,
    pub focused: Option<u32>,
    pub not_responding: Vec<u32>,
    /// Windows that want attention (`helix_window_list_v1.attention`).
    pub attention: Vec<u32>,
    /// The session lock (`Shell::lock`): whether it is locked, whether a
    /// lock surface has the keyboard, and the keys pressed on it.
    pub locked: bool,
    pub lock_focused: bool,
    pub lock_keys: Vec<u32>,
    /// Every `focused`, `workspace` and `geometry` event, in order (window
    /// id 0 / `None`: no focus): what the shell saw on the way, not only
    /// where it ended (#288).
    pub focus_log: Vec<Option<u32>>,
    pub workspace_log: Vec<(u32, u32)>,
    pub minimized_log: Vec<(u32, bool)>,
    /// Launches the shell announced that ended
    /// (`helix_window_list_v1.launch_ended`): startup id and reason.
    pub launches_ended: Vec<(String, u32)>,
    /// The last activation token made (`Shell::token`).
    token: Option<String>,
    lock_configures: Vec<(ext_session_lock_surface_v1::ExtSessionLockSurfaceV1, u32, u32, u32)>,
}

macro_rules! shell_quiet {
    ($($iface:ty),*) => {$(
        impl Dispatch<$iface, ()> for ShellState {
            fn event(_: &mut Self, _: &$iface, _: <$iface as Proxy>::Event, _: &(), _: &WlConnection, _: &QueueHandle<Self>) {}
        }
    )*};
}
shell_quiet!(wl_compositor::WlCompositor, wl_surface::WlSurface, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer, wl_seat::WlSeat, wl_output::WlOutput, ext_session_lock_manager_v1::ExtSessionLockManagerV1, xdg_activation_v1::XdgActivationV1, helix_protocols::gtk_shell_client::gtk_shell1::GtkShell1);

impl Dispatch<xdg_activation_token_v1::XdgActivationTokenV1, ()> for ShellState {
    fn event(state: &mut Self, _: &xdg_activation_token_v1::XdgActivationTokenV1, event: xdg_activation_token_v1::Event, _: &(), _: &WlConnection, _: &QueueHandle<Self>) {
        if let xdg_activation_token_v1::Event::Done { token } = event {
            state.token = Some(token);
        }
    }
}

impl Dispatch<ext_session_lock_v1::ExtSessionLockV1, ()> for ShellState {
    fn event(state: &mut Self, _: &ext_session_lock_v1::ExtSessionLockV1, event: ext_session_lock_v1::Event, _: &(), _: &WlConnection, _: &QueueHandle<Self>) {
        match event {
            ext_session_lock_v1::Event::Locked => state.locked = true,
            ext_session_lock_v1::Event::Finished => state.locked = false,
            _ => {}
        }
    }
}

impl Dispatch<ext_session_lock_surface_v1::ExtSessionLockSurfaceV1, ()> for ShellState {
    fn event(state: &mut Self, surface: &ext_session_lock_surface_v1::ExtSessionLockSurfaceV1, event: ext_session_lock_surface_v1::Event, _: &(), _: &WlConnection, _: &QueueHandle<Self>) {
        if let ext_session_lock_surface_v1::Event::Configure { serial, width, height } = event {
            state.lock_configures.push((surface.clone(), serial, width, height));
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for ShellState {
    fn event(state: &mut Self, _: &wl_keyboard::WlKeyboard, event: wl_keyboard::Event, _: &(), _: &WlConnection, _: &QueueHandle<Self>) {
        match event {
            wl_keyboard::Event::Enter { .. } => state.lock_focused = true,
            wl_keyboard::Event::Leave { .. } => state.lock_focused = false,
            wl_keyboard::Event::Key { key, state: WEnum::Value(wl_keyboard::KeyState::Pressed), .. } => state.lock_keys.push(key),
            _ => {}
        }
    }
}

/// The session lock the test holds as the shell (ext-session-lock-v1).
pub struct Lock {
    lock: ext_session_lock_v1::ExtSessionLockV1,
    surfaces: Vec<(wl_surface::WlSurface, ext_session_lock_surface_v1::ExtSessionLockSurfaceV1)>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ShellState {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &WlConnection, _: &QueueHandle<Self>) {}
}

impl ShellState {
    fn entry(&mut self, id: u32) -> &mut Listed {
        if let Some(i) = self.windows.iter().position(|w| w.id == id) {
            return &mut self.windows[i];
        }
        self.windows.push(Listed { id, ..Listed::default() });
        self.windows.last_mut().unwrap()
    }
}

impl Dispatch<HelixWindowListV1, ()> for ShellState {
    fn event(state: &mut Self, _: &HelixWindowListV1, event: helix_window_list_v1::Event, _: &(), _: &WlConnection, _: &QueueHandle<Self>) {
        use helix_window_list_v1::Event as E;
        match event {
            E::Window { id, app_id, title } => {
                let entry = state.entry(id);
                entry.app_id = app_id;
                entry.title = title;
            }
            E::Closed { id } => {
                state.windows.retain(|w| w.id != id);
                state.attention.retain(|i| *i != id);
                state.not_responding.retain(|i| *i != id);
            }
            E::Icon { id, size, .. } => state.entry(id).icon_size = size,
            E::Focused { id } => {
                state.focused = (id != 0).then_some(id);
                state.focus_log.push(state.focused);
            }
            E::Workspace { id, workspace } => state.workspace_log.push((id, workspace)),
            E::Geometry { id, x, y, width, height, minimized } => {
                let entry = state.entry(id);
                entry.geometry = Some((x, y, width, height));
                entry.minimized = minimized != 0;
                state.minimized_log.push((id, minimized != 0));
            }
            E::Attention { id, wanting } => {
                state.attention.retain(|i| *i != id);
                if wanting != 0 {
                    state.attention.push(id);
                }
            }
            E::NotResponding { id, responding } => {
                state.not_responding.retain(|i| *i != id);
                if responding == 0 {
                    state.not_responding.push(id);
                }
            }
            E::LaunchEnded { startup_id, reason } => {
                let reason = match reason {
                    WEnum::Value(r) => r as u32,
                    WEnum::Unknown(r) => r,
                };
                state.launches_ended.push((startup_id, reason));
            }
            _ => {}
        }
    }
}

/// The test, as the session's shell.
pub struct Shell {
    queue: EventQueue<ShellState>,
    pub state: ShellState,
    list: HelixWindowListV1,
    globals: GlobalList,
    dir: PathBuf,
}

impl Drop for Shell {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Shell {
    fn new(stream: UnixStream, dir: PathBuf) -> Self {
        let conn = WlConnection::from_socket(stream).unwrap();
        let (globals, mut queue) = registry_queue_init::<ShellState>(&conn).unwrap();
        let qh = queue.handle();
        let list: HelixWindowListV1 = globals.bind(&qh, 1..=14, ()).expect("the shell is offered the window list");
        let mut state = ShellState::default();
        queue.roundtrip(&mut state).unwrap();
        Shell { queue, state, list, globals, dir }
    }

    pub fn roundtrip(&mut self) {
        self.queue.roundtrip(&mut self.state).unwrap();
    }

    /// Wait until `done` holds of the window list, or fail with `what`.
    pub fn until(&mut self, what: &str, done: impl Fn(&ShellState) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.roundtrip();
            if done(&self.state) {
                return;
            }
            assert!(Instant::now() < deadline, "{what}: {:?}", self.state.windows);
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The listed window titled `title`, once it is listed.
    pub fn window(&mut self, title: &str) -> Listed {
        self.until(&format!("no window titled {title:?} is listed"), |s| s.windows.iter().any(|w| w.title == title));
        self.state.windows.iter().find(|w| w.title == title).cloned().unwrap()
    }

    /// Do window-list `action` (helix_window_list_v1.action) to window `id`.
    pub fn action(&mut self, id: u32, action: u32) {
        self.list.window_action(id, helix_window_list_v1::Action::try_from(action).expect("a window-list action"));
        self.roundtrip();
    }

    /// Lock the session as the shell's lock screen does: a lock surface
    /// on every monitor, drawn, with the keyboard on it.
    pub fn lock(&mut self) -> Lock {
        let qh = self.queue.handle();
        let compositor: wl_compositor::WlCompositor = self.globals.bind(&qh, 4..=6, ()).unwrap();
        let shm: wl_shm::WlShm = self.globals.bind(&qh, 1..=1, ()).unwrap();
        let seat: wl_seat::WlSeat = self.globals.bind(&qh, 5..=9, ()).unwrap();
        seat.get_keyboard(&qh, ());
        let manager: ext_session_lock_manager_v1::ExtSessionLockManagerV1 = self.globals.bind(&qh, 1..=1, ()).expect("the shell is offered the session lock");
        let outputs: Vec<wl_output::WlOutput> = self
            .globals
            .contents()
            .clone_list()
            .into_iter()
            .filter(|g| g.interface == "wl_output")
            .map(|g| self.globals.registry().bind(g.name, g.version.min(4), &qh, ()))
            .collect();
        let lock = manager.lock(&qh, ());
        let surfaces: Vec<_> = outputs
            .iter()
            .map(|output| {
                let surface = compositor.create_surface(&qh, ());
                let lock_surface = lock.get_lock_surface(&surface, output, &qh, ());
                (surface, lock_surface)
            })
            .collect();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !(self.state.locked && self.state.lock_focused) {
            self.roundtrip();
            for (lock_surface, serial, w, h) in std::mem::take(&mut self.state.lock_configures) {
                lock_surface.ack_configure(serial);
                let surface = &surfaces.iter().find(|(_, l)| *l == lock_surface).unwrap().0;
                surface.attach(Some(&lock_buffer(&shm, &qh, (w as i32, h as i32))), 0, 0);
                surface.commit();
            }
            assert!(Instant::now() < deadline, "the session never locked with the keyboard on the lock surface (locked {}, focused {})", self.state.locked, self.state.lock_focused);
            std::thread::sleep(Duration::from_millis(20));
        }
        Lock { lock, surfaces }
    }

    /// Unlock the session (`lock`).
    pub fn unlock(&mut self, lock: Lock) {
        lock.lock.unlock_and_destroy();
        for (surface, lock_surface) in lock.surfaces {
            lock_surface.destroy();
            surface.destroy();
        }
        self.state.locked = false;
        self.roundtrip();
    }

    /// Activate window `id` as the shell does (the window list, the dock).
    pub fn activate(&mut self, id: u32) {
        self.list.activate(id);
        self.roundtrip();
    }

    /// Put window `id` on workspace `workspace` (0-based).
    pub fn move_window(&mut self, id: u32, workspace: u32) {
        self.list.move_window(id, workspace);
        self.roundtrip();
    }

    /// Make workspace `workspace` (0-based) the active one.
    pub fn switch_workspace(&mut self, workspace: u32) {
        self.list.switch_workspace(workspace);
        self.roundtrip();
    }

    /// An activation token, made as the shell makes one for an app it
    /// launches (handed to it as `DESKTOP_STARTUP_ID`).
    pub fn token(&mut self) -> String {
        self.app_token(None)
    }

    /// [`Self::token`] for app `app_id` (a desktop file id, as the shell
    /// sets it).
    fn app_token(&mut self, app_id: Option<&str>) -> String {
        let qh = self.queue.handle();
        let activation: xdg_activation_v1::XdgActivationV1 = self.globals.bind(&qh, 1..=1, ()).expect("helix-comp offers xdg_activation_v1");
        self.state.token = None;
        let token = activation.get_activation_token(&qh, ());
        if let Some(app_id) = app_id {
            token.set_app_id(app_id.to_owned());
        }
        token.commit();
        self.until("no activation token", |s| s.token.is_some());
        token.destroy();
        activation.destroy();
        self.state.token.take().unwrap()
    }

    /// The token of a launch of app `app_id`, as the shell makes it when
    /// it launches an app: announced as a launch's
    /// (`gtk_shell1.notify_launch`).
    pub fn launch_token(&mut self, app_id: Option<&str>) -> String {
        use helix_protocols::gtk_shell_client::gtk_shell1::GtkShell1;
        let token = self.app_token(app_id);
        let qh = self.queue.handle();
        let gtk_shell: GtkShell1 = self.globals.bind(&qh, 3..=3, ()).expect("helix-comp offers gtk_shell1");
        gtk_shell.notify_launch(token.clone());
        self.roundtrip();
        token
    }

    /// How many times window `title` is listed.
    pub fn listed(&mut self, title: &str) -> usize {
        self.roundtrip();
        self.state.windows.iter().filter(|w| w.title == title).count()
    }
}

/// Window-list actions used here.
pub mod action {
    pub const CLOSE: u32 = 0;
    pub const MINIMIZE: u32 = 1;
    pub const MAXIMIZE: u32 = 2;
    pub const UNMAXIMIZE: u32 = 3;
    pub const TOGGLE_FULLSCREEN: u32 = 7;
    pub const FORCE_QUIT: u32 = 31;
    pub const RAISE: u32 = 25;
    pub const LOWER: u32 = 26;
    pub const SHOW_DESKTOP: u32 = 28;
}

/// A stand-in shell program (`--shell`): it hands its Wayland socket to
/// the test, which then talks as the shell.
fn stand_in_shell(dir: &Path) -> (PathBuf, UnixListener) {
    let path = dir.join("handover");
    let listener = UnixListener::bind(&path).unwrap();
    let script = dir.join("shell");
    let python = format!(
        "import os,socket,time\ns=socket.socket(socket.AF_UNIX)\ns.connect({path:?})\nsocket.send_fds(s,[b'x'],[int(os.environ['WAYLAND_SOCKET'])])\ntime.sleep(3600)\n",
        path = path.to_str().unwrap()
    );
    std::fs::write(&script, format!("#!/bin/sh\nexec /usr/bin/python3 -I -c \"$(cat <<'EOF'\n{python}EOF\n)\"\n")).unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    (script, listener)
}

/// The shell's socket, from the stand-in.
fn shell_socket(listener: &UnixListener) -> UnixStream {
    let (stream, _) = listener.accept().unwrap();
    let mut byte = [0u8; 1];
    let mut space = [0u8; 64];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
    // SAFETY: a zeroed msghdr pointing at the buffers above, which outlive
    // the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = space.as_mut_ptr().cast();
    msg.msg_controllen = space.len() as _;
    // SAFETY: as above; the descriptor is ours to own once received.
    unsafe {
        assert!(libc::recvmsg(std::os::fd::AsRawFd::as_raw_fd(&stream), &mut msg, 0) >= 0, "no socket from the stand-in shell");
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        assert!(!cmsg.is_null() && (*cmsg).cmsg_type == libc::SCM_RIGHTS, "no descriptor from the stand-in shell");
        let fd = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<i32>());
        UnixStream::from(OwnedFd::from_raw_fd(fd))
    }
}

/// A plain buffer of `size` for a lock surface.
fn lock_buffer(shm: &wl_shm::WlShm, qh: &QueueHandle<ShellState>, (w, h): (i32, i32)) -> wl_buffer::WlBuffer {
    let bytes: Vec<u8> = std::iter::repeat_n(0xFF20_2020u32.to_le_bytes(), (w * h) as usize).flatten().collect();
    // SAFETY: memfd_create returns a new fd we own (or -1).
    let raw = unsafe { libc::memfd_create(c"lock-test".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(raw >= 0);
    // SAFETY: just created.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    std::io::Write::write_all(&mut std::fs::File::from(fd.try_clone().unwrap()), &bytes).unwrap();
    let pool = shm.create_pool(std::os::fd::AsFd::as_fd(&fd), bytes.len() as i32, qh, ());
    let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, ());
    pool.destroy();
    buffer
}
