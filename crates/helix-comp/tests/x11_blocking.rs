//! An Xwayland that is busy, stuck, or held by a client doesn't stop the
//! desktop: Helix's window manager never waits for an X reply on the
//! compositor's thread (#223; catalog X81, W87). A reply it waited for
//! would stop Wayland clients, input and drawing until Xwayland answered,
//! and Xwayland itself waits on the compositor at times.
//!
//! Runs the real `helix-comp` headless; needs `dbus-daemon` and Xwayland
//! (as every headless session), skipped otherwise.

mod common;

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::xwm::{self, X};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;

/// How long a Wayland round trip of a fresh client takes, or `None` if it
/// takes longer than `limit` (the compositor is stuck). The client runs
/// on its own thread, so a stuck compositor can't stop the test.
fn wayland_round_trip(display: PathBuf, limit: Duration) -> Option<Duration> {
    struct Nothing;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let start = Instant::now();
        let Ok(stream) = UnixStream::connect(&display) else { return };
        let Ok(conn) = wayland_client::Connection::from_socket(stream) else { return };
        let mut queue = conn.new_event_queue::<Nothing>();
        if queue.roundtrip(&mut Nothing).is_ok() {
            let _ = tx.send(start.elapsed());
        }
    });
    rx.recv_timeout(limit).ok()
}

/// X81: while an X11 client holds the server (XGrabServer: ImageMagick's
/// `import`, xmag-like tools), Wayland clients still get answers although
/// the window manager has things to ask X about: a window's title
/// changed, a menu appeared, and the clipboard was asked for. (Xwayland
/// keeps answering its window manager during grabs; this guards that the
/// desktop still does too.) Once the grab ends everything caught up.
#[test]
fn a_held_x_server_does_not_stop_the_desktop() {
    let Some((session, mut shell)) = xwm::session_with_shell("x11-grab") else { return };
    let display = PathBuf::from(&session.wayland_display);
    let app = X::new(&session);
    let window = app.mapped_titled("before");
    shell.window("before");
    // A Wayland copy, for the X11 app to paste during the grab.
    let mut wayland = common::connect(&session);
    wayland.copy("text/plain;charset=utf-8", b"held");

    let grabber = X::new(&session);
    let (net_wm_name, clipboard, utf8, into) = (grabber.atom("_NET_WM_NAME"), grabber.atom("CLIPBOARD"), grabber.atom("UTF8_STRING"), grabber.atom("HELIX_TEST"));
    let requestor = grabber.create((0, 0), (1, 1));
    grabber.conn.grab_server().unwrap();
    // Only the grabbing client's requests run now; the window manager
    // hears of each and has to ask X about it.
    grabber.conn.change_property8(x11rb::protocol::xproto::PropMode::REPLACE, window, net_wm_name, utf8, b"during").unwrap();
    let menu = grabber.popup((20, 20), (80, 40));
    grabber.conn.convert_selection(requestor, clipboard, utf8, into, x11rb::CURRENT_TIME).unwrap();
    grabber.flush();
    // Time for the window manager to hear of it all.
    std::thread::sleep(Duration::from_millis(300));

    let mut slow = Vec::new();
    for _ in 0..5 {
        match wayland_round_trip(display.clone(), Duration::from_millis(500)) {
            Some(took) => slow.extend((took > Duration::from_millis(500)).then_some(took)),
            None => slow.push(Duration::MAX),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Let go before failing, so the session can end.
    grabber.conn.ungrab_server().unwrap();
    grabber.flush();
    assert!(slow.is_empty(), "Wayland clients got no answer while an X11 client held the server: {slow:?}");

    // Caught up: the title, the clipboard, and the menu (shown, still
    // not a managed window).
    shell.until("the title changed during the grab never reached the window list", |s| s.windows.iter().any(|w| w.title == "during"));
    let deadline = Instant::now() + Duration::from_secs(5);
    let answered = loop {
        wayland.roundtrip();
        if let Some(Event::SelectionNotify(e)) = grabber.conn.poll_for_event().unwrap() {
            break e;
        }
        assert!(Instant::now() < deadline, "the X11 app's paste during the grab was never answered");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(answered.requestor, requestor);
    assert!(!app.client_list().contains(&menu), "the menu is listed as an app window");
    xwm::wm_still_works(&session);
}

/// The Xwayland of `session`, by the display it serves.
fn xwayland_pid(session: &helix_test_support::headless::HeadlessSession) -> Option<u32> {
    std::fs::read_dir("/proc").ok()?.flatten().find_map(|entry| {
        let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
        let cmdline = std::fs::read(entry.path().join("cmdline")).ok()?;
        let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
        let is_xwayland = args.first().is_some_and(|a| a.ends_with(b"Xwayland"));
        (is_xwayland && args.contains(&session.x_display.as_bytes())).then_some(pid)
    })
}

fn signal(pid: u32, signal: &str) {
    let _ = std::process::Command::new("kill").args([signal, &pid.to_string()]).status();
}

/// W87: Xwayland stops answering (busy, stuck, or waiting on the
/// compositor itself) while a Wayland drag comes over an X11 window, which
/// the window manager has to ask X about (does the window take drops?).
/// The pointer still moves, Wayland clients still get answers, and once
/// Xwayland goes on the drag does too.
#[test]
fn a_stuck_xwayland_does_not_stop_the_desktop() {
    if common::known_failure(&[common::Xwm::Smithay], "Smithay's window manager waits for X replies on the compositor's thread (#223)") {
        return;
    }
    let Some(mut session) = xwm::session() else { return };
    let Some(xwayland) = xwayland_pid(&session) else { panic!("no Xwayland for {}", session.x_display) };
    let display = PathBuf::from(&session.wayland_display);
    let x = X::new(&session);
    let target = x.create((400, 300), (300, 200));
    x.map_managed(target);
    let mut app = common::connect(&session);
    let (window, ()) = app.toplevel((200, 150), |_, _| {});
    let surface = window.surface.clone();
    let start = common::point_over(&mut session, &mut app, &surface, (800, 600));
    // A point over the X11 window that the Wayland one doesn't cover.
    let ((tx, ty), (tw, th)) = (x.position(target), x.size(target));
    let (tx, ty, tw, th) = (i32::from(tx), i32::from(ty), i32::from(tw), i32::from(th));
    let over_x = (tx + 10..tx + tw - 10).step_by(20).flat_map(|px| (ty + 10..ty + th - 10).step_by(20).map(move |py| (px, py))).find(|(px, py)| {
        session.input(&format!("move {px} {py}"));
        app.roundtrip();
        let inside = x.conn.query_pointer(target).ok().and_then(|c| c.reply().ok()).is_some_and(|r| (0..tw).contains(&i32::from(r.win_x)) && (0..th).contains(&i32::from(r.win_y)));
        app.state.pointer.is_none() && inside
    });
    let Some(over_x) = over_x else { panic!("no point over the X11 window") };
    session.input(&format!("move {} {}", start.0, start.1));
    app.until("the pointer never came back to the Wayland window", |s| s.pointer.as_ref() == Some(&surface));
    session.input("button 272 1");
    app.until("the Wayland window never got the press", |s| s.press.is_some());
    let press = app.state.press.unwrap();
    app.drag("text/plain;charset=utf-8", b"stuck", &surface, press);

    // Xwayland stops; it goes on again after 3 s whatever happens, so a
    // stuck compositor shows as a failure here, not as a hung test.
    signal(xwayland, "-STOP");
    let watchdog = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(3));
        signal(xwayland, "-CONT");
    });
    let moved = Instant::now();
    session.input(&format!("move {} {}", over_x.0, over_x.1));
    session.input(&format!("move {} {}", over_x.0 + 1, over_x.1 + 1));
    let move_took = moved.elapsed();
    let round_trip = wayland_round_trip(display, Duration::from_millis(500));
    signal(xwayland, "-CONT");
    watchdog.join().unwrap();
    assert!(move_took < Duration::from_millis(500), "the pointer was stuck for {move_took:?} while Xwayland didn't answer");
    assert!(round_trip.is_some(), "Wayland clients got no answer while Xwayland didn't answer");

    // Xwayland goes on: so does the drag, and the window manager works.
    session.input("button 272 0");
    app.roundtrip();
    xwm::wm_still_works(&session);
}

/// #249: Helix's other X11 connection (pings and icons, `x11ping`) must
/// not wait for Xwayland on the compositor's thread either. With Xwayland
/// stopped (busy or stuck), the shell activates an X11 window: Helix then
/// checks its app answers pings, which needs to know whether the window
/// takes pings. The activation is answered at once, not after Xwayland
/// comes back. (A client holding the X server doesn't stop Helix's
/// connections: Xwayland answers them during XGrabServer.)
#[test]
fn a_stuck_xwayland_does_not_stop_pings() {
    let Some((session, mut shell)) = xwm::session_with_shell("x11-stuck-ping") else { return };
    let Some(xwayland) = xwayland_pid(&session) else { panic!("no Xwayland for {}", session.x_display) };
    let app = X::new(&session);
    app.mapped_titled("first");
    let first = shell.window("first");
    app.mapped_titled("second");
    let second = shell.window("second");
    shell.activate(second.id);
    shell.until("the second window never got focus", |s| s.focused == Some(second.id));

    signal(xwayland, "-STOP");
    let watchdog = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(3));
        signal(xwayland, "-CONT");
    });
    let asked = Instant::now();
    shell.activate(first.id);
    let took = asked.elapsed();
    signal(xwayland, "-CONT");
    watchdog.join().unwrap();
    assert!(took < Duration::from_millis(500), "activating an X11 window took {took:?} while Xwayland didn't answer");
    shell.until("the first window never got focus", |s| s.focused == Some(first.id));
    xwm::wm_still_works(&session);
}
