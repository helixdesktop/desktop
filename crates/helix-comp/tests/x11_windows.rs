//! How Helix's X11 window manager treats X11 windows (through Xwayland),
//! as an X11 client (this test) sees it:
//!
//! - A window the app hides is marked Withdrawn (`WM_STATE`), as ICCCM
//!   asks; Wine waits for that before it shows a game's window again
//!   (#163).
//! - A window shown and hidden at once stays hidden, and nothing loops
//!   (#162; Zoom's screen-sharing toolbar does this).
//!
//! Runs the real `helix-comp` headless; needs `dbus-daemon` and Xwayland
//! (as every headless session), skipped otherwise.

mod common;

use std::time::{Duration, Instant};

use helix_test_support::headless::{HeadlessSession, Options};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, MapState, CreateWindowAux, EventMask, UnmapNotifyEvent, Window, WindowClass, UNMAP_NOTIFY_EVENT};
use x11rb::rust_connection::RustConnection;

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
