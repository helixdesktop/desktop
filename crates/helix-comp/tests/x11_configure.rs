//! Where X11 windows are and how big, as the apps see it: the edge cases of
//! `tests/x11_cases.md` about ConfigureRequest replies, root-relative
//! positions, size hints, states set before mapping, `_GTK_FRAME_EXTENTS`,
//! sync requests and `_NET_WM_MOVERESIZE` (#191).
//!
//! Runs the real `helix-comp` headless; needs `dbus-daemon` and Xwayland
//! (as every headless session), skipped otherwise.

mod common;

use std::time::Duration;

use common::xwm::{self, until, X};
use x11rb::connection::Connection;
use x11rb::properties::WmSizeHints;
use x11rb::protocol::xproto::{AtomEnum, ConfigureNotifyEvent, ConfigureWindowAux, ConnectionExt, StackMode, Window};
use x11rb::protocol::Event;

const MAXIMIZED: [&str; 2] = ["_NET_WM_STATE_MAXIMIZED_HORZ", "_NET_WM_STATE_MAXIMIZED_VERT"];

/// The next ConfigureNotify `window` gets, within 5 seconds.
fn next_configure(x: &X, window: Window, what: &str) -> ConfigureNotifyEvent {
    x.wait_event(what, |e| match e {
        Event::ConfigureNotify(c) if c.window == window => Some(*c),
        _ => None,
    })
}

/// The last ConfigureNotify `window` got in the next `time`.
fn last_configure(x: &X, window: Window, time: Duration) -> Option<ConfigureNotifyEvent> {
    x.events_for(time).into_iter().rev().find_map(|e| match e {
        Event::ConfigureNotify(c) if c.window == window => Some(c),
        _ => None,
    })
}

/// Where and how big the window really is (on the root).
fn rect(x: &X, window: Window) -> (i16, i16, u16, u16) {
    let (px, py) = x.position(window);
    let (w, h) = x.size(window);
    (px, py, w, h)
}

/// X35: a request the window manager does not follow (a maximized window
/// asking for another size) is still answered, with a ConfigureNotify of
/// where the window is (ICCCM 4.1.5); apps wait for one.
#[test]
fn a_refused_request_is_answered() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.mapped();
    x.net_wm_state(window, 1, &MAXIMIZED);
    until("the window was not maximized", || x.size(window).0 == xwm::SCREEN.0);
    std::thread::sleep(Duration::from_millis(300));
    x.drain();
    x.conn.configure_window(window, &ConfigureWindowAux::new().width(300).height(200)).unwrap();
    x.flush();
    let answer = next_configure(&x, window, "the refused request got no ConfigureNotify");
    let (px, py, w, h) = rect(&x, window);
    assert_eq!((answer.x, answer.y, answer.width, answer.height), (px, py, w, h), "the answer is not where the window is");
}

/// X35: an app window asking to move (which Helix places itself) gets an
/// answer too.
#[test]
fn a_refused_move_is_answered() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.mapped();
    std::thread::sleep(Duration::from_millis(300));
    x.drain();
    let (px, py) = x.position(window);
    x.conn.configure_window(window, &ConfigureWindowAux::new().x(i32::from(px) + 37).y(i32::from(py) + 23)).unwrap();
    x.flush();
    let answer = next_configure(&x, window, "the move request got no ConfigureNotify");
    let (nx, ny) = x.position(window);
    assert_eq!((answer.x, answer.y), (nx, ny), "the answer is not where the window is");
}

/// X36: a request only says what it changes: asking for a width leaves
/// the height and position alone.
#[test]
fn a_request_changes_only_what_it_asks() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.mapped();
    std::thread::sleep(Duration::from_millis(300));
    let (px, py, _, h) = rect(&x, window);
    x.conn.configure_window(window, &ConfigureWindowAux::new().width(321)).unwrap();
    x.flush();
    until("the width did not change", || x.size(window).0 == 321);
    assert_eq!(rect(&x, window), (px, py, 321, h), "more than the width changed");
}

/// X37: raising or lowering itself (Wine) gets an answer, whatever the
/// window manager decides, and odd stacking modes do no harm.
#[test]
fn stacking_requests_are_answered() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let other = x.mapped();
    let window = x.mapped();
    std::thread::sleep(Duration::from_millis(300));
    x.drain();
    x.conn.configure_window(window, &ConfigureWindowAux::new().stack_mode(StackMode::BELOW)).unwrap();
    x.flush();
    next_configure(&x, window, "a stacking request got no ConfigureNotify");
    for mode in [StackMode::TOP_IF, StackMode::BOTTOM_IF, StackMode::OPPOSITE, StackMode::ABOVE] {
        x.conn.configure_window(window, &ConfigureWindowAux::new().stack_mode(mode).sibling(other)).unwrap();
    }
    x.flush();
    xwm::wm_still_works(&session);
}

/// X38: the window is told where it really is on the root, every time it
/// is put somewhere (tooltips and dropdowns are placed from it; Firefox's
/// address bar dropdown was misplaced).
#[test]
fn the_window_is_told_where_it_is() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.window();
    x.map(window);
    until("the window manager never managed the window", || x.managed(window));
    let last = last_configure(&x, window, Duration::from_millis(500));
    let (px, py, w, h) = rect(&x, window);
    if let Some(last) = last {
        assert_eq!((last.x, last.y, last.width, last.height), (px, py, w, h), "the last ConfigureNotify is not where the window is");
    }
    x.net_wm_state(window, 1, &MAXIMIZED);
    until("the window was not maximized", || x.size(window).0 == xwm::SCREEN.0);
    let last = last_configure(&x, window, Duration::from_millis(500)).expect("no ConfigureNotify when maximized");
    let (px, py, w, h) = rect(&x, window);
    assert_eq!((last.x, last.y, last.width, last.height), (px, py, w, h), "the last ConfigureNotify is not where the maximized window is");
}

/// X39: sized before it is shown (apps size themselves first): it shows
/// at that size.
#[test]
fn a_size_asked_for_before_mapping_is_kept() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.window();
    x.conn.configure_window(window, &ConfigureWindowAux::new().width(333).height(222)).unwrap();
    x.map_managed(window);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(x.size(window), (333, 222));
}

/// X40: maximized before it is shown: the app is told its final size
/// before it shows, so it does not draw at the wrong size first.
#[test]
#[ignore = "fails on Smithay's xwm: a window maximized before mapping is mapped first and resized after (#191)"]
fn maximized_before_mapping_gets_its_size_first() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.window();
    x.set_atoms(window, "_NET_WM_STATE", &MAXIMIZED);
    x.map(window);
    let mut first = None;
    x.wait_event("the window never mapped", |e| match e {
        Event::ConfigureNotify(c) if c.window == window && first.is_none() => {
            first = Some(*c);
            None
        }
        Event::MapNotify(m) if m.window == window => Some(()),
        _ => None,
    });
    until("the window was not maximized", || x.size(window).0 == xwm::SCREEN.0);
    let first = first.expect("no ConfigureNotify before the window mapped");
    assert_eq!((first.width, first.height), x.size(window), "the first size is not the maximized one");
}

/// X41: size hints: a base size and no minimum is the minimum (ICCCM).
#[test]
#[ignore = "fails on Smithay's xwm: a base size without a minimum size is not taken as the minimum (#191)"]
fn the_base_size_stands_in_for_a_minimum() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.window();
    let mut hints = WmSizeHints::new();
    hints.base_size = Some((200, 100));
    hints.set_normal_hints(&x.conn, window).unwrap();
    x.map_managed(window);
    x.conn.configure_window(window, &ConfigureWindowAux::new().width(50).height(40)).unwrap();
    x.flush();
    std::thread::sleep(Duration::from_millis(500));
    let (w, h) = x.size(window);
    assert!(w >= 200 && h >= 100, "{w}×{h}, below the base size");
}

/// X41: a minimum size is kept (Helix clamps requests to the hints).
#[test]
fn the_minimum_size_is_kept() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.window();
    let mut hints = WmSizeHints::new();
    hints.min_size = Some((220, 120));
    hints.set_normal_hints(&x.conn, window).unwrap();
    x.map_managed(window);
    x.conn.configure_window(window, &ConfigureWindowAux::new().width(50).height(40)).unwrap();
    x.flush();
    std::thread::sleep(Duration::from_millis(500));
    let (w, h) = x.size(window);
    assert!(w >= 220 && h >= 120, "{w}×{h}, below the minimum size");
}

/// X42: a GTK window drawing its own shadow (`_GTK_FRAME_EXTENTS`)
/// maximized: its visible part fills the screen, the shadow beyond it.
#[test]
#[ignore = "fails on Smithay's xwm: a maximized window's shadow (_GTK_FRAME_EXTENTS) is kept on screen (#191)"]
fn a_maximized_window_puts_its_shadow_off_screen() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.window();
    // left, right, top, bottom; and no title bar from the window manager.
    x.set32(window, "_GTK_FRAME_EXTENTS", AtomEnum::CARDINAL, &[10, 10, 8, 12]);
    let motif = x.atom("_MOTIF_WM_HINTS");
    x.set32(window, "_MOTIF_WM_HINTS", motif, &[2, 0, 0, 0, 0]);
    x.map_managed(window);
    x.net_wm_state(window, 1, &MAXIMIZED);
    until("the window was not maximized", || x.size(window).0 >= xwm::SCREEN.0);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(rect(&x, window), (-10, -8, xwm::SCREEN.0 + 20, xwm::SCREEN.1 + 20), "the maximized window with its shadow");
}

/// X43: `_NET_WM_SYNC_REQUEST`: an app that takes part is told before
/// each resize, so resizes don't show half-drawn frames.
#[test]
#[ignore = "fails on Smithay's xwm: no _NET_WM_SYNC_REQUEST before a resize (#191)"]
fn resizes_are_announced_to_apps_that_sync() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    x11rb::protocol::sync::ConnectionExt::sync_initialize(&x.conn, 3, 1).unwrap().reply().unwrap();
    let counter = x.conn.generate_id().unwrap();
    x11rb::protocol::sync::ConnectionExt::sync_create_counter(&x.conn, counter, x11rb::protocol::sync::Int64 { hi: 0, lo: 0 }).unwrap();
    let window = x.window();
    x.set_atoms(window, "WM_PROTOCOLS", &["WM_DELETE_WINDOW", "_NET_WM_SYNC_REQUEST"]);
    x.set32(window, "_NET_WM_SYNC_REQUEST_COUNTER", AtomEnum::CARDINAL, &[counter]);
    x.map_managed(window);
    std::thread::sleep(Duration::from_millis(300));
    x.drain();
    x.net_wm_state(window, 1, &MAXIMIZED);
    let (wm_protocols, sync) = (x.atom("WM_PROTOCOLS"), x.atom("_NET_WM_SYNC_REQUEST"));
    let mut announced = false;
    x.wait_event("the window was never resized", |e| match e {
        Event::ClientMessage(m) if m.window == window && m.type_ == wm_protocols && m.data.as_data32()[0] == sync => {
            announced = true;
            None
        }
        Event::ConfigureNotify(c) if c.window == window && c.width == xwm::SCREEN.0 => Some(()),
        _ => None,
    });
    assert!(announced, "no _NET_WM_SYNC_REQUEST before the resize");
}

/// X44: `_NET_WM_MOVERESIZE` from an app's own title bar (Steam,
/// Chromium): move (8) moves the window with the pointer; unknown
/// directions are ignored.
#[test]
fn an_app_moves_its_window_from_its_own_title_bar() {
    let Some(mut session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.mapped();
    std::thread::sleep(Duration::from_millis(300));
    x.tell_wm(window, "_NET_WM_MOVERESIZE", [0, 0, 42, 1, 1]);
    let (px, py) = x.position(window);
    let (gx, gy) = (i32::from(px) + 50, i32::from(py) + 30);
    session.input(&format!("move {gx} {gy}"));
    session.input("button 272 1");
    x.tell_wm(window, "_NET_WM_MOVERESIZE", [gx as u32, gy as u32, 8, 1, 1]);
    std::thread::sleep(Duration::from_millis(200));
    for step in 1..=10 {
        session.input(&format!("move {} {}", gx + step * 10, gy + step * 5));
    }
    session.input("button 272 0");
    until("the window did not follow the pointer", || x.position(window) == (px + 100, py + 50));
}

/// X75 (S16): X11 coordinates at an Xwayland scale (Helix scales Xwayland
/// on HiDPI screens; always by a whole number, see `xwayland_scale`):
/// the X screen is that much bigger, and the pointer and windows agree
/// on where things are, both ways.
#[test]
fn coordinates_at_an_xwayland_scale_of_two() {
    let comp = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-comp"));
    let settings = "[org/gnome/mutter/wayland]\nxwayland-scaling-factor=2.0\n";
    let options = helix_test_support::headless::Options { comp: &comp, shell: None, size: (u32::from(xwm::SCREEN.0), u32::from(xwm::SCREEN.1)), settings, env: Vec::new() };
    let Some(mut session) = helix_test_support::headless::HeadlessSession::start(options) else { return };
    if session.x_display.is_empty() {
        return;
    }
    let x = X::new(&session);
    until("the X screen never became twice as big", || {
        let g = x.conn.get_geometry(x.root).unwrap().reply().unwrap();
        (g.width, g.height) == (xwm::SCREEN.0 * 2, xwm::SCREEN.1 * 2)
    });
    // A menu at an X position, and the pointer moved on the screen.
    let menu = x.create_with(600, 400, 200, 200, x11rb::protocol::xproto::CreateWindowAux::new().override_redirect(1));
    let events = x11rb::protocol::xproto::EventMask::STRUCTURE_NOTIFY | x11rb::protocol::xproto::EventMask::ENTER_WINDOW | x11rb::protocol::xproto::EventMask::LEAVE_WINDOW | x11rb::protocol::xproto::EventMask::POINTER_MOTION;
    x.conn.change_window_attributes(menu, &x11rb::protocol::xproto::ChangeWindowAttributesAux::new().event_mask(events)).unwrap();
    x.conn.map_window(menu).unwrap();
    x.flush();
    std::thread::sleep(Duration::from_millis(300));
    // Screen (350, 250) is X (700, 500): inside the menu.
    session.input("move 350 250");
    let (rx, ry) = x.wait_event("the menu never got the pointer", |e| match e {
        Event::EnterNotify(n) if n.event == menu => Some((n.root_x, n.root_y)),
        Event::MotionNotify(n) if n.event == menu => Some((n.root_x, n.root_y)),
        _ => None,
    });
    assert_eq!((rx, ry), (700, 500), "the pointer at screen (350, 250) is at X {:?}", (rx, ry));
    // Screen (290, 190) is X (580, 380): outside it.
    x.events_for(Duration::from_millis(100));
    session.input("move 290 190");
    x.wait_event("the menu never lost the pointer", |e| match e {
        Event::LeaveNotify(n) if n.event == menu => Some(()),
        _ => None,
    });
    // A managed window is where the window manager says, in X pixels.
    let window = x.window();
    x.map_managed(window);
    let (px, py) = x.position(window);
    let (w, h) = x.size(window);
    assert_eq!((w, h), (200, 100), "the window's size in X pixels");
    assert!(px % 2 == 0 && py % 2 == 0, "a managed window at an odd X position {:?} at scale 2", (px, py));
}
