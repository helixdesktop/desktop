//! X11 apps that place themselves (#226), as mutter: an undecorated
//! window covering a monitor exactly is full screen there (how SDL, Unity
//! and Wine titles do borderless full screen), and a window moving itself
//! after it is shown (Wine's SetWindowPos, installers, launchers) goes
//! there, kept on a monitor. Under either window manager (`HELIX_XWM`).

mod common;

use common::xwm::{session, until, X, SCREEN};
use x11rb::protocol::xproto::{ConfigureWindowAux, ConnectionExt as _, CreateWindowAux};

/// `_MOTIF_WM_HINTS` asking for no decorations.
fn undecorated(x: &X, window: u32) {
    let motif = x.atom("_MOTIF_WM_HINTS");
    x.set32(window, "_MOTIF_WM_HINTS", motif, &[2, 0, 0, 0, 0]);
}

/// An undecorated window created covering the monitor shows full screen.
#[test]
fn an_undecorated_window_covering_the_monitor_is_full_screen() {
    let Some(session) = session() else { return };
    let x = X::new(&session);
    let window = x.create_with(0, 0, SCREEN.0, SCREEN.1, CreateWindowAux::new());
    undecorated(&x, window);
    x.map_managed(window);
    until("the window covering the monitor is not full screen", || x.has_state(window, "_NET_WM_STATE_FULLSCREEN"));
    assert_eq!((x.position(window), x.size(window)), ((0, 0), SCREEN), "the full-screen window doesn't cover the monitor");
}

/// An undecorated window that resizes itself to cover the monitor goes
/// full screen, and leaves it when it resizes itself back.
#[test]
fn an_undecorated_window_resized_to_the_monitor_goes_full_screen_and_back() {
    let Some(session) = session() else { return };
    let x = X::new(&session);
    let window = x.create_with(10, 10, 300, 200, CreateWindowAux::new());
    undecorated(&x, window);
    x.map_managed(window);
    until("the window was not placed", || x.size(window) == (300, 200));
    let cover = ConfigureWindowAux::new().x(0).y(0).width(u32::from(SCREEN.0)).height(u32::from(SCREEN.1));
    x.conn.configure_window(window, &cover).unwrap();
    x.flush();
    until("the window covering the monitor did not go full screen", || x.has_state(window, "_NET_WM_STATE_FULLSCREEN"));
    let back = ConfigureWindowAux::new().x(100).y(100).width(300).height(200);
    x.conn.configure_window(window, &back).unwrap();
    x.flush();
    until("the window resized back is still full screen", || !x.has_state(window, "_NET_WM_STATE_FULLSCREEN"));
    until("the window resized back is not its size", || x.size(window) == (300, 200));
}

/// A decorated window covering the monitor is just a big window.
#[test]
fn a_decorated_window_covering_the_monitor_is_not_full_screen() {
    let Some(session) = session() else { return };
    let x = X::new(&session);
    let window = x.create_with(0, 0, SCREEN.0, SCREEN.1, CreateWindowAux::new());
    x.map_managed(window);
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!x.has_state(window, "_NET_WM_STATE_FULLSCREEN"), "a decorated window went full screen");
}

/// A shown window moving itself goes where it asks.
#[test]
fn a_window_moving_itself_goes_there() {
    let Some(session) = session() else { return };
    let x = X::new(&session);
    let window = x.mapped();
    x.conn.configure_window(window, &ConfigureWindowAux::new().x(200).y(150)).unwrap();
    x.flush();
    until("the window did not move where it asked", || x.position(window) == (200, 150));
}

/// A window moving itself off the screen stays on it, its title bar
/// reachable.
#[test]
fn a_window_moving_itself_off_screen_stays_on_it() {
    let Some(session) = session() else { return };
    let x = X::new(&session);
    let window = x.mapped();
    let before = x.position(window);
    x.conn.configure_window(window, &ConfigureWindowAux::new().x(-5000).y(-5000)).unwrap();
    x.flush();
    until("the window did not move", || x.position(window) != before);
    let (px, py) = x.position(window);
    let (w, _) = x.size(window);
    assert!(i32::from(px) + i32::from(w) > 0 && py > 0, "the window went off the screen: {:?}", (px, py));
}
