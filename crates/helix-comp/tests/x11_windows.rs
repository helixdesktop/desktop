//! How Helix's X11 window manager treats X11 windows (through Xwayland),
//! as an X11 client (this test) sees it:
//!
//! - A window the app hides is marked Withdrawn (`WM_STATE`), as ICCCM
//!   asks; Wine waits for that before it shows a game's window again
//!   (#163).
//! - A window shown and hidden at once stays hidden, and nothing loops
//!   (#162; Zoom's screen-sharing toolbar does this).
//! - Typing reaches an app whose focused window takes no input
//!   (`WM_HINTS` input false): the X server hands the keys to the window
//!   the app gave the X focus (#164; JetBrains IDEs' popups).
//! - A fullscreen or maximized window keeps its size when the app asks
//!   for another (#165; games set their video mode's): it gets that size
//!   when restored.
//! - `_NET_ACTIVE_WINDOW` names the focused X11 window (none while a
//!   Wayland window has focus), and asking for it activates a window for
//!   a pager or an app moving between its own windows; others only want
//!   attention (#166).
//! - The compositor gives its X display back when it exits: no lock is
//!   left for the next X server to take over (#181).
//!
//! Runs the real `helix-comp` headless; needs `dbus-daemon` and Xwayland
//! (as every headless session), skipped otherwise.

mod common;

use std::time::{Duration, Instant};

use helix_test_support::headless::{HeadlessSession, Options};
use x11rb::connection::Connection;
use x11rb::properties::WmHints;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt, InputFocus, MapState, PropMode, CreateWindowAux, EventMask, UnmapNotifyEvent, Window, WindowClass, UNMAP_NOTIFY_EVENT};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

fn session() -> Option<HeadlessSession> {
    let comp = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-comp"));
    let session = HeadlessSession::start(Options { comp: &comp, shell: None, size: (800, 600), settings: "", env: Vec::new() })?;
    if session.x_display.is_empty() {
        eprintln!("no Xwayland; skipped");
        return None;
    }
    Some(session)
}

struct X11 {
    conn: RustConnection,
    root: Window,
}

impl X11 {
    fn new(session: &HeadlessSession) -> Self {
        let (conn, screen_num) = common::x11(session);
        let root = conn.setup().roots[screen_num].root;
        X11 { conn, root }
    }

    fn atom(&self, name: &str) -> u32 {
        self.conn.intern_atom(false, name.as_bytes()).unwrap().reply().unwrap().atom
    }

    /// A plain top-level window, not yet mapped.
    fn window(&self) -> Window {
        let screen = &self.conn.setup().roots[0];
        let window = self.conn.generate_id().unwrap();
        let aux = CreateWindowAux::new().background_pixel(screen.white_pixel).event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE);
        self.conn.create_window(0, window, self.root, 10, 10, 200, 100, 0, WindowClass::INPUT_OUTPUT, 0, &aux).unwrap();
        self.conn.flush().unwrap();
        window
    }

    /// The window's `WM_STATE` state (1 Normal, 0 Withdrawn), if set.
    fn wm_state(&self, window: Window) -> Option<u32> {
        let wm_state = self.atom("WM_STATE");
        let reply = self.conn.get_property(false, window, wm_state, AtomEnum::ANY, 0, 2).unwrap().reply().unwrap();
        reply.value32().and_then(|mut v| v.next())
    }

    /// Hide `window` as `XWithdrawWindow` does: unmap it, and tell the
    /// window manager with a synthetic UnmapNotify (ICCCM 4.1.4).
    fn withdraw(&self, window: Window) {
        self.conn.unmap_window(window).unwrap();
        let notify = UnmapNotifyEvent { response_type: UNMAP_NOTIFY_EVENT, sequence: 0, event: self.root, window, from_configure: false };
        self.conn.send_event(false, self.root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, notify).unwrap();
        self.conn.flush().unwrap();
    }

    /// The window's size.
    fn size(&self, window: Window) -> (u16, u16) {
        let g = self.conn.get_geometry(window).unwrap().reply().unwrap();
        (g.width, g.height)
    }

    /// Ask the window manager to add (1) or remove (0) `states`
    /// (`_NET_WM_STATE`).
    fn net_wm_state(&self, window: Window, action: u32, states: &[&str]) {
        let state = self.atom("_NET_WM_STATE");
        let a = self.atom(states[0]);
        let b = states.get(1).map_or(0, |s| self.atom(s));
        let message = ClientMessageEvent::new(32, window, state, [action, a, b, 1, 0]);
        self.conn.send_event(false, self.root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, message).unwrap();
        self.conn.flush().unwrap();
    }

    /// The root's `_NET_ACTIVE_WINDOW`.
    fn active(&self) -> Option<Window> {
        let active = self.atom("_NET_ACTIVE_WINDOW");
        let reply = self.conn.get_property(false, self.root, active, AtomEnum::WINDOW, 0, 1).unwrap().reply().unwrap();
        reply.value32().and_then(|mut v| v.next())
    }

    /// Ask for `window` to be activated, from `source` (1 an app, 2 a
    /// pager), the app's active window being `current`.
    fn activate(&self, window: Window, source: u32, current: Window) {
        let active = self.atom("_NET_ACTIVE_WINDOW");
        let message = ClientMessageEvent::new(32, window, active, [source, x11rb::CURRENT_TIME, current, 0, 0]);
        self.conn.send_event(false, self.root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, message).unwrap();
        self.conn.flush().unwrap();
    }

    /// A mapped window, once the window manager manages it.
    fn mapped(&self) -> Window {
        let window = self.window();
        self.conn.map_window(window).unwrap();
        self.conn.flush().unwrap();
        self.until("the window manager never managed the window", |x| x.wm_state(window) == Some(1));
        window
    }

    /// Wait until `done` holds, or fail with `what`.
    fn until(&self, what: &str, mut done: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(self) {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
fn a_hidden_window_is_withdrawn() {
    let Some(session) = session() else { return };
    let x = X11::new(&session);
    let window = x.window();
    x.conn.map_window(window).unwrap();
    x.conn.flush().unwrap();
    x.until("the window manager never managed the window", |x| x.wm_state(window) == Some(1));

    x.withdraw(window);
    x.until("WM_STATE stayed Normal after the window was hidden", |x| x.wm_state(window) == Some(0));
}

#[test]
fn a_window_shown_and_hidden_at_once_stays_hidden() {
    let Some(session) = session() else { return };
    let x = X11::new(&session);
    let window = x.window();
    // Before the window manager has handled the MapRequest.
    x.conn.map_window(window).unwrap();
    x.withdraw(window);

    std::thread::sleep(Duration::from_millis(500));
    while x.conn.poll_for_event().unwrap().is_some() {}
    // A second of quiet: the window manager reparenting and remapping it
    // over and over shows here as a stream of events.
    let mut events = 0;
    let quiet = Instant::now() + Duration::from_secs(1);
    while Instant::now() < quiet {
        if x.conn.poll_for_event().unwrap().is_some() {
            events += 1;
        } else {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let attributes = x.conn.get_window_attributes(window).unwrap().reply().unwrap();
    assert_eq!(attributes.map_state, MapState::UNMAPPED, "the hidden window is shown ({events} events in a second)");
    assert!(events < 10, "{events} events in a second after the window was hidden");
    assert_ne!(x.wm_state(window), Some(1), "WM_STATE is still Normal");
}

#[test]
fn typing_reaches_an_app_whose_focused_window_takes_no_input() {
    let Some(mut session) = session() else { return };
    let x = X11::new(&session);
    let hints = |window: Window, input: bool| {
        let mut hints = WmHints::new();
        hints.input = Some(input);
        hints.set(&x.conn, window).unwrap();
    };
    let watch = |window: Window| {
        let aux = x11rb::protocol::xproto::ChangeWindowAttributesAux::new().event_mask(EventMask::KEY_PRESS | EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE);
        x.conn.change_window_attributes(window, &aux).unwrap();
    };
    // The main window, and a popup for it that takes no input.
    let main = x.window();
    watch(main);
    hints(main, true);
    x.conn.map_window(main).unwrap();
    x.conn.flush().unwrap();
    x.until("the window manager never managed the main window", |x| x.wm_state(main) == Some(1));
    let popup = x.window();
    watch(popup);
    hints(popup, false);
    x.conn.change_property32(PropMode::REPLACE, popup, AtomEnum::WM_TRANSIENT_FOR, AtomEnum::WINDOW, &[main]).unwrap();
    x.conn.map_window(popup).unwrap();
    x.conn.flush().unwrap();
    x.until("the window manager never managed the popup", |x| x.wm_state(popup) == Some(1));
    // The app keeps the X focus on its main window, as Java does.
    x.conn.set_input_focus(InputFocus::PARENT, main, x11rb::CURRENT_TIME).unwrap();
    x.conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    while x.conn.poll_for_event().unwrap().is_some() {}

    session.input("type abc");
    let mut keys = 0;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match x.conn.poll_for_event().unwrap() {
            Some(Event::KeyPress(press)) if press.event == main => keys += 1,
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    assert_eq!(keys, 3, "key presses the main window got");
}

#[test]
fn a_fullscreen_or_maximized_window_keeps_its_size() {
    let Some(session) = session() else { return };
    let x = X11::new(&session);
    let ask = |window: Window, (w, h): (u32, u32)| {
        x.conn.configure_window(window, &ConfigureWindowAux::new().width(w).height(h)).unwrap();
        x.conn.flush().unwrap();
        // Time for the window manager to (wrongly) follow.
        std::thread::sleep(Duration::from_millis(500));
    };

    let window = x.window();
    x.conn.map_window(window).unwrap();
    x.conn.flush().unwrap();
    x.until("the window manager never managed the window", |x| x.wm_state(window) == Some(1));
    let maximized = ["_NET_WM_STATE_MAXIMIZED_HORZ", "_NET_WM_STATE_MAXIMIZED_VERT"];
    x.net_wm_state(window, 1, &maximized);
    x.until("the window was not maximized", |x| x.size(window).0 == 800);
    let size = x.size(window);
    ask(window, (300, 200));
    assert_eq!(x.size(window), size, "the maximized window took the size the app asked for");
    x.net_wm_state(window, 0, &maximized);
    x.until("the restored window does not have the size the app asked for", |x| x.size(window) == (300, 200));

    x.net_wm_state(window, 1, &["_NET_WM_STATE_FULLSCREEN"]);
    x.until("the window was not fullscreen", |x| x.size(window) == (800, 600));
    ask(window, (640, 480));
    assert_eq!(x.size(window), (800, 600), "the fullscreen window took the size the app asked for");
}

#[test]
fn the_active_window_follows_focus() {
    let Some(session) = session() else { return };
    let x = X11::new(&session);
    let a = x.mapped();
    x.until("_NET_ACTIVE_WINDOW is not the new window", |x| x.active() == Some(a));
    let b = x.mapped();
    x.until("_NET_ACTIVE_WINDOW is not the newer window", |x| x.active() == Some(b));

    // A Wayland window takes focus: no X11 window is active.
    let mut app = common::connect(&session);
    let (window, ()) = app.toplevel((200, 100), |_, _| {});
    let surface = window.surface.clone();
    app.until("the Wayland window has no focus", |s| s.keyboard.as_ref() == Some(&surface));
    x.until("_NET_ACTIVE_WINDOW still names an X11 window", |x| x.active() == Some(x11rb::NONE));
}

#[test]
fn activation_requests() {
    let Some(session) = session() else { return };
    let x = X11::new(&session);
    let a = x.mapped();
    let b = x.mapped();
    x.until("the newer window has no focus", |x| x.active() == Some(b));

    // A pager (wmctrl -a) activates.
    x.activate(a, 2, x11rb::NONE);
    x.until("a pager's request was not granted", |x| x.active() == Some(a));
    // An app out of the blue does not take focus.
    x.activate(b, 1, x11rb::NONE);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(x.active(), Some(a), "an app's request out of the blue took focus");
    // An app moving from its focused window to another does.
    x.activate(b, 1, a);
    x.until("an app's request from its focused window was not granted", |x| x.active() == Some(b));
}

#[test]
fn the_display_is_released_at_exit() {
    let Some(session) = session() else { return };
    let number = session.x_display.trim_start_matches(':').to_owned();
    let lock = std::path::PathBuf::from(format!("/tmp/.X{number}-lock"));
    assert!(lock.exists(), "no {}", lock.display());
    drop(session);
    assert!(!lock.exists(), "{} is left behind", lock.display());
}
