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
//! - `_NET_WM_STATE`: Always on Top asked for (and shown back), a dialog
//!   made modal before or after it is shown, fullscreen asked for before
//!   mapping, and helper windows left out of the window list
//!   (`SKIP_TASKBAR`) (#167).
//! - Splash screens, notifications and docks (`_NET_WM_WINDOW_TYPE`) get
//!   no title bar and no focus: splash screens centered, the others where
//!   they ask (#168).
//! - Over X11 windows that set no cursor, the pointer is the cursor
//!   theme's at `cursor-size`, not Helix's built-in arrow (#171).
//! - Xwayland gone (crashed, killed): it is started again on the same
//!   display, and X11 apps run again (#169).
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
    if common::no_xwayland(&session) {
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

    /// The window's `_NET_WM_STATE` atoms.
    fn net_states(&self, window: Window) -> Vec<u32> {
        let state = self.atom("_NET_WM_STATE");
        let reply = self.conn.get_property(false, window, state, AtomEnum::ATOM, 0, 64).unwrap().reply().unwrap();
        reply.value32().map(|v| v.collect()).unwrap_or_default()
    }

    /// A window with `_NET_WM_STATE` `states` and `title` set before it
    /// maps, transient for `parent` if any; mapped and managed.
    fn mapped_with(&self, title: &str, states: &[&str], parent: Option<Window>) -> Window {
        let window = self.window();
        self.conn.change_property8(PropMode::REPLACE, window, AtomEnum::WM_NAME, AtomEnum::STRING, title.as_bytes()).unwrap();
        let atoms: Vec<u32> = states.iter().map(|s| self.atom(s)).collect();
        let state = self.atom("_NET_WM_STATE");
        self.conn.change_property32(PropMode::REPLACE, window, state, AtomEnum::ATOM, &atoms).unwrap();
        if let Some(parent) = parent {
            self.conn.change_property32(PropMode::REPLACE, window, AtomEnum::WM_TRANSIENT_FOR, AtomEnum::WINDOW, &[parent]).unwrap();
        }
        self.conn.map_window(window).unwrap();
        self.conn.flush().unwrap();
        self.until("the window manager never managed the window", |x| x.wm_state(window) == Some(1));
        window
    }

    /// A `width`×`height` window at (`x`, `y`) of `_NET_WM_WINDOW_TYPE`
    /// `kind`, asking for that position (`USPosition`); mapped and managed.
    fn mapped_typed(&self, title: &str, kind: &str, (x, y): (i16, i16), (width, height): (u16, u16)) -> Window {
        let screen = &self.conn.setup().roots[0];
        let window = self.conn.generate_id().unwrap();
        let aux = CreateWindowAux::new().background_pixel(screen.white_pixel).event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE);
        self.conn.create_window(0, window, self.root, x, y, width, height, 0, WindowClass::INPUT_OUTPUT, 0, &aux).unwrap();
        self.conn.change_property8(PropMode::REPLACE, window, AtomEnum::WM_NAME, AtomEnum::STRING, title.as_bytes()).unwrap();
        let (types, kind) = (self.atom("_NET_WM_WINDOW_TYPE"), self.atom(kind));
        self.conn.change_property32(PropMode::REPLACE, window, types, AtomEnum::ATOM, &[kind]).unwrap();
        let mut hints = x11rb::properties::WmSizeHints::new();
        hints.position = Some((x11rb::properties::WmSizeHintsSpecification::UserSpecified, i32::from(x), i32::from(y)));
        hints.set_normal_hints(&self.conn, window).unwrap();
        self.conn.map_window(window).unwrap();
        self.conn.flush().unwrap();
        self.until("the window manager never managed the window", |x| x.wm_state(window) == Some(1));
        window
    }

    /// Where the window is on the root window.
    fn position(&self, window: Window) -> (i16, i16) {
        let t = self.conn.translate_coordinates(window, self.root, 0, 0).unwrap().reply().unwrap();
        (t.dst_x, t.dst_y)
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
    // Until a key gets through (the popup focused and paired with its
    // surface takes a moment, longer under load), not a fixed wait (#269).
    let ready = Instant::now() + Duration::from_secs(10);
    let probe = 'through: loop {
        assert!(Instant::now() < ready, "no key ever reached the app's focused main window");
        session.input("type x");
        let until = Instant::now() + Duration::from_millis(200);
        while Instant::now() < until {
            match x.conn.poll_for_event().unwrap() {
                Some(Event::KeyPress(press)) if press.event == main => break 'through press.detail,
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    };
    while x.conn.poll_for_event().unwrap().is_some() {}

    // Three keys, not counting a late probe key.
    session.input("type abc");
    let mut keys = 0;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match x.conn.poll_for_event().unwrap() {
            Some(Event::KeyPress(press)) if press.event == main && press.detail != probe => keys += 1,
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
    let ours = std::fs::read_to_string(&lock).unwrap_or_else(|_| panic!("no {}", lock.display()));
    drop(session);
    // Gone, or another server's by now: tests running alongside start
    // sessions too, and one may take the display the moment it's free.
    let now = std::fs::read_to_string(&lock).ok();
    assert!(now.as_deref() != Some(ours.as_str()), "{} is left behind ({})", lock.display(), ours.trim());
}

#[test]
fn always_on_top() {
    let Some(session) = session() else { return };
    let x = X11::new(&session);
    let above = x.atom("_NET_WM_STATE_ABOVE");
    let window = x.mapped();
    x.net_wm_state(window, 1, &["_NET_WM_STATE_ABOVE"]);
    x.until("Always on Top was not shown back", |x| x.net_states(window).contains(&above));
    x.net_wm_state(window, 0, &["_NET_WM_STATE_ABOVE"]);
    x.until("Always on Top stayed after it was taken back", |x| !x.net_states(window).contains(&above));
    x.net_wm_state(window, 2, &["_NET_WM_STATE_ABOVE"]);
    x.until("toggling Always on Top did nothing", |x| x.net_states(window).contains(&above));
}

#[test]
fn modal_dialogs_take_their_parents_focus() {
    let Some(session) = session() else { return };
    let x = X11::new(&session);
    // Made modal before it is shown, as most toolkits do.
    let parent = x.mapped();
    let dialog = x.mapped_with("dialog", &["_NET_WM_STATE_MODAL"], Some(parent));
    x.activate(parent, 2, x11rb::NONE);
    std::thread::sleep(Duration::from_millis(300));
    x.until("focus went to the parent, not its modal dialog", |x| x.active() == Some(dialog));

    // Made modal once shown (gtk_window_set_modal on a shown window).
    let parent = x.mapped();
    let dialog = x.mapped_with("dialog", &[], Some(parent));
    x.activate(parent, 2, x11rb::NONE);
    x.until("the parent did not get focus", |x| x.active() == Some(parent));
    x.net_wm_state(dialog, 1, &["_NET_WM_STATE_MODAL"]);
    x.until("focus stayed on the parent of a dialog made modal", |x| x.active() == Some(dialog));
}

#[test]
fn fullscreen_asked_for_before_mapping() {
    let Some(session) = session() else { return };
    let x = X11::new(&session);
    let window = x.mapped_with("game", &["_NET_WM_STATE_FULLSCREEN"], None);
    x.until("the window did not go fullscreen", |x| x.size(window) == (800, 600));
    let fullscreen = x.atom("_NET_WM_STATE_FULLSCREEN");
    assert!(x.net_states(window).contains(&fullscreen));
}

#[test]
fn splash_screens_notifications_and_docks() {
    let Some(session) = session() else { return };
    let x = X11::new(&session);
    let app = x.mapped();
    x.until("the app window has no focus", |x| x.active() == Some(app));

    let splash = x.mapped_typed("splash", "_NET_WM_WINDOW_TYPE_SPLASH", (10, 10), (300, 150));
    x.until("the splash screen is not centered without a frame", |x| x.position(splash) == (250, 225));
    let notification = x.mapped_typed("toast", "_NET_WM_WINDOW_TYPE_NOTIFICATION", (480, 20), (300, 80));
    x.until("the notification is not where it asked", |x| x.position(notification) == (480, 20));
    // Kept on the output.
    let dock = x.mapped_typed("dock", "_NET_WM_WINDOW_TYPE_DOCK", (0, 590), (800, 40));
    x.until("the dock is not kept on the output", |x| x.position(dock) == (0, 560));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(x.active(), Some(app), "a splash screen, notification or dock took focus");

    // A notification moves itself.
    x.conn.configure_window(notification, &ConfigureWindowAux::new().x(480).y(120)).unwrap();
    x.conn.flush().unwrap();
    x.until("the notification did not move where it asked", |x| x.position(notification) == (480, 120));
}

#[test]
fn x11_windows_show_the_cursor_theme() {
    // The default settings' theme and size; without it installed (as on
    // CI) the fallback arrow is right.
    let Some(path) = xcursor::CursorTheme::load("Adwaita").load_icon("default") else {
        eprintln!("no Adwaita cursor theme; skipped");
        return;
    };
    let images = xcursor::parser::parse_xcursor(&std::fs::read(path).unwrap()).unwrap();
    let nominal = images.iter().map(|i| i.size).min_by_key(|s| s.abs_diff(24)).unwrap();
    let theme = images.iter().find(|i| i.size == nominal).unwrap();
    let Some(mut session) = session() else { return };
    let x = X11::new(&session);
    let window = x.mapped();
    let (wx, wy) = x.position(window);
    session.input(&format!("move {} {}", wx + 100, wy + 50));
    x11rb::protocol::xfixes::ConnectionExt::xfixes_query_version(&x.conn, 5, 0).unwrap().reply().unwrap();
    x.until("the pointer is not the theme's cursor", |x| {
        let image = x11rb::protocol::xfixes::ConnectionExt::xfixes_get_cursor_image(&x.conn).unwrap().reply().unwrap();
        (u32::from(image.width), u32::from(image.height), u32::from(image.xhot), u32::from(image.yhot)) == (theme.width, theme.height, theme.xhot, theme.yhot)
    });
}

/// The session's Xwayland process (`Xwayland :N ...`).
fn xwayland_pid(display: &str) -> Option<i32> {
    std::fs::read_dir("/proc").ok()?.flatten().find_map(|entry| {
        let pid: i32 = entry.file_name().to_str()?.parse().ok()?;
        let cmdline = std::fs::read(entry.path().join("cmdline")).ok()?;
        let mut args = cmdline.split(|b| *b == 0);
        let program = args.next()?;
        let ours = program.ends_with(b"Xwayland") && args.next() == Some(display.as_bytes());
        ours.then_some(pid)
    })
}

#[test]
fn xwayland_is_started_again() {
    let Some(session) = session() else { return };
    let before = xwayland_pid(&session.x_display).expect("the session's Xwayland");
    {
        let x = X11::new(&session);
        x.mapped();
        // SAFETY: signalling the Xwayland of this test's own session.
        assert_eq!(unsafe { libc::kill(before, libc::SIGKILL) }, 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        while x.conn.get_input_focus().ok().and_then(|c| c.reply().ok()).is_some() {
            assert!(Instant::now() < deadline, "the X connection outlived Xwayland");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    // Back on the same display (DISPLAY is exported), with a cookie in
    // the same file.
    let deadline = Instant::now() + Duration::from_secs(10);
    while xwayland_pid(&session.x_display).is_none_or(|pid| pid == before) {
        assert!(Instant::now() < deadline, "Xwayland was not started again on {}", session.x_display);
        std::thread::sleep(Duration::from_millis(50));
    }
    let x = loop {
        let (conn, _) = match std::panic::catch_unwind(|| common::x11(&session)) {
            Ok(c) => c,
            Err(_) => {
                assert!(Instant::now() < deadline, "cannot connect to the new Xwayland");
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        drop(conn);
        break X11::new(&session);
    };
    // Managed again.
    x.mapped();
}

/// Another X server (another session's Xwayland) looking for a display
/// while Xwayland is being started again doesn't get the session's: it
/// is kept, and Xwayland comes back on it (X11 apps and the session keep
/// DISPLAY). The other server is played by this test, taking the display
/// as X servers and Smithay do: its lock file created, or taken over
/// from a process that is gone.
#[test]
fn xwayland_comes_back_on_its_own_display() {
    let Some(session) = session() else { return };
    let number: u32 = session.x_display.trim_start_matches(':').parse().expect("a display number");
    let lock = format!("/tmp/.X{number}-lock");
    let killed = common::xwm::kill_xwayland(&session);
    let deadline = Instant::now() + Duration::from_secs(10);
    let back = loop {
        assert!(Instant::now() < deadline, "Xwayland was not started again");
        let taken = match std::fs::OpenOptions::new().write(true).create_new(true).open(&lock) {
            Ok(_) => true,
            Err(_) => std::fs::read_to_string(&lock).ok().and_then(|pid| pid.trim().parse::<i32>().ok()).is_some_and(|pid| {
                // SAFETY: kill(2) with signal 0 only checks the process.
                unsafe { libc::kill(pid, 0) != 0 }
            }),
        };
        if taken {
            let _ = std::fs::remove_file(&lock);
            panic!("another X server could take the session's display while Xwayland was started again");
        }
        if let Some(pid) = common::xwm::xwayland_pid(&session).filter(|pid| *pid != killed) {
            break pid;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(back > 0);
    common::xwm::restarted_xwayland(&session, killed).mapped();
}

// ---- the window list, as the shell sees it ----

mod shell {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;

    use helix_protocols::client::window_list::helix_window_list_v1::{self, HelixWindowListV1};
    use wayland_client::globals::{registry_queue_init, GlobalListContents};
    use wayland_client::protocol::wl_registry;
    use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};

    #[derive(Default)]
    pub struct Listed {
        pub windows: Vec<(u32, String)>,
    }

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Listed {
        fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
    }

    impl Dispatch<HelixWindowListV1, ()> for Listed {
        fn event(state: &mut Self, _: &HelixWindowListV1, event: helix_window_list_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            match event {
                helix_window_list_v1::Event::Window { id, title, .. } => {
                    state.windows.retain(|(i, _)| *i != id);
                    state.windows.push((id, title));
                }
                helix_window_list_v1::Event::Closed { id } => state.windows.retain(|(i, _)| *i != id),
                _ => {}
            }
        }
    }

    /// A stand-in shell (`--shell`): it hands its Wayland socket to the
    /// test, which then talks as the shell.
    pub fn program(dir: &Path) -> (std::path::PathBuf, UnixListener) {
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
    pub fn socket(listener: &UnixListener) -> UnixStream {
        let (stream, _) = listener.accept().unwrap();
        let mut byte = [0u8; 1];
        let mut space = [0u8; 64];
        let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
        // SAFETY: a zeroed msghdr pointing at the buffers above, which
        // outlive the call.
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

    pub fn window_list(stream: UnixStream) -> (EventQueue<Listed>, Listed, HelixWindowListV1) {
        let conn = Connection::from_socket(stream).unwrap();
        let (globals, mut queue) = registry_queue_init::<Listed>(&conn).unwrap();
        let qh = queue.handle();
        let list: HelixWindowListV1 = globals.bind(&qh, 1..=1, ()).expect("the shell is offered the window list");
        let mut listed = Listed::default();
        queue.roundtrip(&mut listed).unwrap();
        (queue, listed, list)
    }
}

#[test]
fn helper_windows_stay_out_of_the_window_list() {
    let comp = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-comp"));
    let dir = std::env::temp_dir().join(format!("helix-skip-taskbar-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (program, listener) = shell::program(&dir);
    let Some(session) = HeadlessSession::start(Options { comp: &comp, shell: Some(&program), size: (800, 600), settings: "", env: Vec::new() }) else { return };
    if common::no_xwayland(&session) {
        return;
    }
    let (mut queue, mut listed, _list) = shell::window_list(shell::socket(&listener));
    let x = X11::new(&session);
    x.mapped_with("listed", &[], None);
    x.mapped_with("helper", &["_NET_WM_STATE_SKIP_TASKBAR"], None);
    x.mapped_typed("toast", "_NET_WM_WINDOW_TYPE_NOTIFICATION", (480, 20), (300, 80));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !listed.windows.iter().any(|(_, t)| t == "listed") {
        assert!(Instant::now() < deadline, "the window never showed in the list: {:?}", listed.windows);
        queue.roundtrip(&mut listed).unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    queue.roundtrip(&mut listed).unwrap();
    assert!(!listed.windows.iter().any(|(_, t)| t == "helper" || t == "toast"), "a helper or notification window is listed: {:?}", listed.windows);
    drop(session);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_window_list_drops_x11_windows_when_xwayland_goes() {
    let comp = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-comp"));
    let dir = std::env::temp_dir().join(format!("helix-xwayland-gone-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (program, listener) = shell::program(&dir);
    let Some(session) = HeadlessSession::start(Options { comp: &comp, shell: Some(&program), size: (800, 600), settings: "", env: Vec::new() }) else { return };
    if common::no_xwayland(&session) {
        return;
    }
    let (mut queue, mut listed, _list) = shell::window_list(shell::socket(&listener));
    let x = X11::new(&session);
    x.mapped_with("x11 app", &[], None);
    let until = |queue: &mut wayland_client::EventQueue<shell::Listed>, listed: &mut shell::Listed, what: &str, done: &dyn Fn(&shell::Listed) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(listed) {
            assert!(Instant::now() < deadline, "{what}: {:?}", listed.windows);
            queue.roundtrip(listed).unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    until(&mut queue, &mut listed, "the X11 window is not listed", &|l| l.windows.iter().any(|(_, t)| t == "x11 app"));
    let pid = xwayland_pid(&session.x_display).expect("the session's Xwayland");
    // SAFETY: signalling the Xwayland of this test's own session.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    until(&mut queue, &mut listed, "the dead X11 window is still listed", &|l| !l.windows.iter().any(|(_, t)| t == "x11 app"));
    drop(session);
    let _ = std::fs::remove_dir_all(&dir);
}
