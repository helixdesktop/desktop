//! Drag and drop between X11 apps (through Xwayland) and Wayland apps
//! (#170), as the apps see it:
//!
//! - An X11 app drags (XDND) onto a Wayland window: the Wayland app gets
//!   the offer and the data (an image from GIMP into Files).
//! - A Wayland app drags onto an X11 window: the X11 app gets XDND and
//!   the data (files from Files into a Wine or Java app).
//! - An X11 app drags onto another X11 app's window, still straight
//!   between them (Smithay's window manager steps aside).
//!
//! Runs the real `helix-comp` headless; needs `dbus-daemon` and Xwayland
//! (as every headless session), skipped otherwise.

mod common;

use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use helix_test_support::headless::{HeadlessSession, Options};
use wayland_client::protocol::wl_data_device_manager::DndAction;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ClientMessageEvent, ConnectionExt, CreateWindowAux, EventMask, PropMode, SelectionNotifyEvent, Window, WindowClass, SELECTION_NOTIFY_EVENT};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

const BTN_LEFT: u32 = 0x110;
const XDND_VERSION: u32 = 5;
const DATA: &[u8] = b"dragged words";

fn session() -> Option<HeadlessSession> {
    let comp = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-comp"));
    let session = HeadlessSession::start(Options { comp: &comp, shell: None, size: (800, 600), settings: "", env: Vec::new() })?;
    if session.x_display.is_empty() {
        eprintln!("no Xwayland; skipped");
        return None;
    }
    Some(session)
}

#[derive(Clone, Copy)]
struct Atoms {
    aware: u32,
    selection: u32,
    enter: u32,
    position: u32,
    status: u32,
    leave: u32,
    drop: u32,
    finished: u32,
    copy: u32,
    utf8: u32,
    wm_state: u32,
}

/// An X11 client with one top-level window of `size`, mapped and managed.
struct X11 {
    conn: Arc<RustConnection>,
    root: Window,
    window: Window,
    atoms: Atoms,
}

impl X11 {
    fn new(session: &HeadlessSession, (width, height): (u16, u16), aware: bool) -> Self {
        let (conn, screen_num) = common::x11(session);
        let conn = Arc::new(conn);
        let screen = &conn.setup().roots[screen_num];
        let (root, white) = (screen.root, screen.white_pixel);
        let atom = |name: &str| conn.intern_atom(false, name.as_bytes()).unwrap().reply().unwrap().atom;
        let atoms = Atoms {
            aware: atom("XdndAware"),
            selection: atom("XdndSelection"),
            enter: atom("XdndEnter"),
            position: atom("XdndPosition"),
            status: atom("XdndStatus"),
            leave: atom("XdndLeave"),
            drop: atom("XdndDrop"),
            finished: atom("XdndFinished"),
            copy: atom("XdndActionCopy"),
            utf8: atom("UTF8_STRING"),
            wm_state: atom("WM_STATE"),
        };
        let window = conn.generate_id().unwrap();
        let events = EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION | EventMask::ENTER_WINDOW;
        let aux = CreateWindowAux::new().background_pixel(white).event_mask(events);
        conn.create_window(0, window, root, 10, 10, width, height, 0, WindowClass::INPUT_OUTPUT, 0, &aux).unwrap();
        if aware {
            conn.change_property32(PropMode::REPLACE, window, atoms.aware, AtomEnum::ATOM, &[XDND_VERSION]).unwrap();
        }
        conn.map_window(window).unwrap();
        conn.flush().unwrap();
        let x = X11 { conn, root, window, atoms };
        let deadline = Instant::now() + Duration::from_secs(5);
        while x.wm_state() != Some(1) {
            assert!(Instant::now() < deadline, "the window manager never managed the window");
            std::thread::sleep(Duration::from_millis(20));
        }
        x
    }

    fn wm_state(&self) -> Option<u32> {
        let reply = self.conn.get_property(false, self.window, self.atoms.wm_state, AtomEnum::ANY, 0, 2).unwrap().reply().unwrap();
        reply.value32().and_then(|mut v| v.next())
    }

    /// The window's rectangle on the root window.
    fn rect(&self) -> (i32, i32, i32, i32) {
        let t = self.conn.translate_coordinates(self.window, self.root, 0, 0).unwrap().reply().unwrap();
        let g = self.conn.get_geometry(self.window).unwrap().reply().unwrap();
        (i32::from(t.dst_x), i32::from(t.dst_y), i32::from(g.width), i32::from(g.height))
    }
}

fn inside((x, y, w, h): (i32, i32, i32, i32), (px, py): (i32, i32)) -> bool {
    px >= x && py >= y && px < x + w && py < y + h
}

/// A point well inside `a` and outside `b`.
fn point_in_not(a: (i32, i32, i32, i32), b: Option<(i32, i32, i32, i32)>) -> (i32, i32) {
    let (x, y, w, h) = a;
    (x + 10..x + w - 10)
        .step_by(10)
        .flat_map(|px| (y + 10..y + h - 10).step_by(10).map(move |py| (px, py)))
        .find(|p| b.is_none_or(|b| !inside(b, *p)))
        .expect("no point of one window outside the other")
}

fn message(conn: &RustConnection, to: Window, kind: u32, data: [u32; 5]) {
    let event = ClientMessageEvent::new(32, to, kind, data);
    conn.send_event(false, to, EventMask::NO_EVENT, event).unwrap();
    conn.flush().unwrap();
}

/// The XDND-aware window under (`x`, `y`) of the root, looking down from
/// the top-level window there, as toolkits do (not `own`).
fn xdnd_target(x: &X11, (px, py): (i16, i16)) -> Option<Window> {
    let mut window = x.root;
    loop {
        let child = x.conn.translate_coordinates(x.root, window, px, py).ok()?.reply().ok()?.child;
        if child == x11rb::NONE {
            return None;
        }
        if child != x.window {
            let aware = x.conn.get_property(false, child, x.atoms.aware, AtomEnum::ATOM, 0, 1).ok()?.reply().ok()?;
            if aware.value32().and_then(|mut v| v.next()).is_some() {
                return Some(child);
            }
        }
        window = child;
    }
}

/// What the X11 drag source saw.
#[derive(Debug, PartialEq)]
enum SourceSaw {
    /// Positions went to this window.
    Target(Window),
    /// The target accepted (XdndStatus).
    Accepted,
    /// The data was asked for.
    Sent,
    /// The target finished (XdndFinished).
    Finished,
}

/// Run an XDND drag source for `x`'s window: when the button is pressed
/// it owns XdndSelection, follows the pointer (its implicit grab) with
/// XdndPosition to the aware window under it, drops when the button is
/// released, and serves `DATA` as UTF8_STRING.
fn xdnd_source(x: X11) -> mpsc::Receiver<SourceSaw> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let a = x.atoms;
        let mut target: Option<Window> = None;
        while let Ok(event) = x.conn.wait_for_event() {
            match event {
                Event::ButtonPress(press) => {
                    x.conn.set_selection_owner(x.window, a.selection, press.time).unwrap();
                    x.conn.flush().unwrap();
                }
                Event::MotionNotify(motion) => {
                    let time = motion.time;
                    let under = xdnd_target(&x, (motion.root_x, motion.root_y));
                    if under != target {
                        if let Some(old) = target {
                            message(&x.conn, old, a.leave, [x.window, 0, 0, 0, 0]);
                        }
                        if let Some(new) = under {
                            message(&x.conn, new, a.enter, [x.window, XDND_VERSION << 24, a.utf8, 0, 0]);
                            let _ = tx.send(SourceSaw::Target(new));
                        }
                        target = under;
                    }
                    if let Some(t) = target {
                        let at = (u32::from(motion.root_x as u16) << 16) | u32::from(motion.root_y as u16);
                        message(&x.conn, t, a.position, [x.window, 0, at, time, a.copy]);
                    }
                }
                Event::ButtonRelease(release) => {
                    if let Some(t) = target {
                        message(&x.conn, t, a.drop, [x.window, 0, release.time, 0, 0]);
                    }
                }
                Event::ClientMessage(m) if m.type_ == a.status => {
                    if m.data.as_data32()[1] & 1 == 1 {
                        let _ = tx.send(SourceSaw::Accepted);
                    }
                }
                Event::ClientMessage(m) if m.type_ == a.finished => {
                    let _ = tx.send(SourceSaw::Finished);
                }
                Event::SelectionRequest(request) if request.selection == a.selection => {
                    let property = if request.target == a.utf8 {
                        x.conn.change_property8(PropMode::REPLACE, request.requestor, request.property, a.utf8, DATA).unwrap();
                        let _ = tx.send(SourceSaw::Sent);
                        request.property
                    } else {
                        x11rb::NONE
                    };
                    let notify = SelectionNotifyEvent { response_type: SELECTION_NOTIFY_EVENT, sequence: 0, time: request.time, requestor: request.requestor, selection: request.selection, target: request.target, property };
                    x.conn.send_event(false, request.requestor, EventMask::NO_EVENT, notify).unwrap();
                    x.conn.flush().unwrap();
                }
                _ => {}
            }
        }
    });
    rx
}

/// What the X11 drop target saw.
#[derive(Debug, PartialEq)]
enum TargetSaw {
    Entered,
    /// It was dropped on, and fetched this.
    Dropped(Vec<u8>),
}

/// Run an XDND drop target for `x`'s (XDND-aware) window: it accepts the
/// first type offered, and fetches it when dropped on.
fn xdnd_target_app(x: X11) -> mpsc::Receiver<TargetSaw> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let a = x.atoms;
        let mut source = x11rb::NONE;
        let mut kind = a.utf8;
        let property = x.conn.intern_atom(false, b"HELIX_DROPPED").unwrap().reply().unwrap().atom;
        while let Ok(event) = x.conn.wait_for_event() {
            match event {
                Event::ClientMessage(m) if m.type_ == a.enter => {
                    let data = m.data.as_data32();
                    source = data[0];
                    if data[2] != x11rb::NONE {
                        kind = data[2];
                    }
                    let _ = tx.send(TargetSaw::Entered);
                }
                Event::ClientMessage(m) if m.type_ == a.position => {
                    message(&x.conn, source, a.status, [x.window, 1, 0, 0, a.copy]);
                }
                Event::ClientMessage(m) if m.type_ == a.drop => {
                    let time = m.data.as_data32()[2];
                    x.conn.convert_selection(x.window, a.selection, kind, property, time).unwrap();
                    x.conn.flush().unwrap();
                }
                Event::SelectionNotify(n) if n.selection == a.selection => {
                    let data = if n.property == x11rb::NONE {
                        Vec::new()
                    } else {
                        x.conn.get_property(true, x.window, property, AtomEnum::ANY, 0, 1 << 20).unwrap().reply().unwrap().value
                    };
                    message(&x.conn, source, a.finished, [x.window, 1, a.copy, 0, 0]);
                    let _ = tx.send(TargetSaw::Dropped(data));
                }
                _ => {}
            }
        }
    });
    rx
}

/// Wait for `want` among what `rx` reports (others are skipped).
fn expect<T: std::fmt::Debug + PartialEq>(rx: &mpsc::Receiver<T>, want: impl Fn(&T) -> bool, what: &str) -> T {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(seen) if want(&seen) => return seen,
            Ok(_) => {}
            Err(_) => panic!("{what}"),
        }
    }
}

/// Move the pointer to `to` in steps (the drag follows each).
fn glide(session: &mut HeadlessSession, from: (i32, i32), to: (i32, i32)) {
    for i in 1..=8 {
        let x = from.0 + (to.0 - from.0) * i / 8;
        let y = from.1 + (to.1 - from.1) * i / 8;
        session.input(&format!("move {x} {y}"));
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[test]
fn an_x11_app_drags_onto_a_wayland_window() {
    let Some(mut session) = session() else { return };
    let mut app = common::connect(&session);
    let (window, ()) = app.toplevel((300, 200), |_, _| {});
    let surface = window.surface.clone();
    let x = X11::new(&session, (200, 150), false);
    let x_rect = x.rect();
    // Over the Wayland window where the X11 one doesn't cover it.
    let over_app = common::point_over(&mut session, &mut app, &surface, (800, 600));
    let start = point_in_not(x_rect, None);
    let saw = xdnd_source(x);

    session.input(&format!("move {} {}", start.0, start.1));
    std::thread::sleep(Duration::from_millis(200));
    session.input(&format!("button {BTN_LEFT} 1"));
    std::thread::sleep(Duration::from_millis(300));
    glide(&mut session, start, over_app);

    expect(&saw, |s| matches!(s, SourceSaw::Target(_)), "the X11 drag never found a target over the Wayland window");
    app.until("the Wayland window was never offered the drag", |s| s.dnd.is_some());
    let (offer, types, serial) = app.state.dnd.clone().unwrap();
    assert!(types.iter().any(|t| t == "text/plain;charset=utf-8"), "offered {types:?}");
    offer.accept(serial, Some("text/plain;charset=utf-8".to_owned()));
    offer.set_actions(DndAction::Copy, DndAction::Copy);
    app.roundtrip();
    // One more move, for the source to hear it is accepted.
    session.input(&format!("move {} {}", over_app.0 + 1, over_app.1 + 1));
    expect(&saw, |s| *s == SourceSaw::Accepted, "the X11 source never heard the Wayland window accept");

    session.input(&format!("button {BTN_LEFT} 0"));
    app.until("the drag was never dropped on the Wayland window", |s| s.dropped);
    assert_eq!(app.receive(&offer, "text/plain;charset=utf-8"), DATA);
    offer.finish();
    app.roundtrip();
    expect(&saw, |s| *s == SourceSaw::Finished, "the X11 source never heard the drop finished");
}

#[test]
fn a_wayland_app_drags_onto_an_x11_window() {
    let Some(mut session) = session() else { return };
    let x = X11::new(&session, (300, 200), true);
    let x_rect = x.rect();
    let mut app = common::connect(&session);
    let (window, ()) = app.toplevel((200, 150), |_, _| {});
    let surface = window.surface.clone();
    // Over the Wayland window (on top), then over the X11 window where
    // the Wayland one doesn't cover it.
    let start = common::point_over(&mut session, &mut app, &surface, (800, 600));
    let over_x = (x_rect.0 + 10..x_rect.0 + x_rect.2 - 10)
        .step_by(10)
        .flat_map(|px| (x_rect.1 + 10..x_rect.1 + x_rect.3 - 10).step_by(10).map(move |py| (px, py)))
        .find(|(px, py)| {
            session.input(&format!("move {px} {py}"));
            app.roundtrip();
            app.state.pointer.is_none()
        })
        .expect("the Wayland window covers the X11 one");
    let saw = xdnd_target_app(x);

    session.input(&format!("move {} {}", start.0, start.1));
    app.until("the pointer never came back to the Wayland window", |s| s.pointer.as_ref() == Some(&surface));
    session.input(&format!("button {BTN_LEFT} 1"));
    app.until("the Wayland window never got the press", |s| s.press.is_some());
    let press = app.state.press.unwrap();
    app.drag("text/plain;charset=utf-8", DATA, &surface, press);
    glide(&mut session, start, over_x);

    expect(&saw, |s| *s == TargetSaw::Entered, "the X11 window never got XdndEnter");
    session.input(&format!("move {} {}", over_x.0 + 1, over_x.1 + 1));
    std::thread::sleep(Duration::from_millis(200));
    session.input(&format!("button {BTN_LEFT} 0"));
    // The Wayland source serves the data while the X11 app fetches it.
    let deadline = Instant::now() + Duration::from_secs(5);
    let got = loop {
        app.roundtrip();
        match saw.try_recv() {
            Ok(TargetSaw::Dropped(data)) => break data,
            Ok(_) => {}
            Err(_) => {
                assert!(Instant::now() < deadline, "the X11 window was never dropped on");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };
    assert_eq!(got, DATA);
}

#[test]
fn an_x11_app_drags_onto_another_x11_window() {
    let Some(mut session) = session() else { return };
    // Crossed, so that each has a part the other doesn't cover whichever
    // is on top (the press raises the source).
    let source = X11::new(&session, (300, 80), false);
    let target = X11::new(&session, (80, 300), true);
    let (source_rect, target_rect) = (source.rect(), target.rect());
    let start = point_in_not(source_rect, Some(target_rect));
    let over_target = point_in_not(target_rect, Some(source_rect));
    let target_window = target.window;
    // The pointer over each first, so Xwayland knows where they are.
    session.input(&format!("move {} {}", over_target.0, over_target.1));
    std::thread::sleep(Duration::from_millis(200));
    let saw = xdnd_source(source);
    let got = xdnd_target_app(target);

    session.input(&format!("move {} {}", start.0, start.1));
    std::thread::sleep(Duration::from_millis(200));
    session.input(&format!("button {BTN_LEFT} 1"));
    std::thread::sleep(Duration::from_millis(300));
    glide(&mut session, start, over_target);

    // Smithay's drop proxy covers the screen until the drag is over an
    // X11 window; then the source finds the window itself.
    session.input(&format!("move {} {}", over_target.0 + 2, over_target.1 + 2));
    expect(&saw, |s| *s == SourceSaw::Target(target_window), "the X11 drag never found the X11 window (only Smithay's drop proxy?)");
    expect(&got, |s| *s == TargetSaw::Entered, "the X11 target never got XdndEnter");
    session.input(&format!("move {} {}", over_target.0 + 1, over_target.1 + 1));
    std::thread::sleep(Duration::from_millis(200));
    session.input(&format!("button {BTN_LEFT} 0"));
    assert_eq!(expect(&got, |s| matches!(s, TargetSaw::Dropped(_)), "the X11 target was never dropped on"), TargetSaw::Dropped(DATA.to_vec()));
    expect(&saw, |s| *s == SourceSaw::Finished, "the X11 source never heard the drop finished");
}
