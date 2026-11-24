//! Touch on the title bars and borders Helix draws around X11 windows
//! (helixdesktop/spec `contracts/wayland/README.md`, "Window frames";
//! helixdesktop/desktop#295): as with the pointer, and as mutter for its
//! own frames, a finger on the title bar moves the window (a double tap
//! does `action-double-click-titlebar`), on a border resizes it, and a tap
//! on a button presses it (acting only if the finger lifts on it). The
//! touch goes to no app, and never to the window behind the frame.
//!
//! Before the fix only the pointer was looked at: a touch on a frame went
//! to whatever was under it (focusing the window behind) and did nothing.
//!
//! Runs the real `helix-comp` headless; needs `dbus-daemon` and Xwayland
//! (as every headless session), skipped otherwise.

mod common;

use std::time::Duration;

use common::xwm::{self, until, X};
use helix_test_support::headless::HeadlessSession;
use x11rb::protocol::xproto::{ConfigureWindowAux, ConnectionExt as _, Window};
use x11rb::protocol::Event;

/// Helix's title bar height (`helix_comp::frames::TITLE_H`).
const TITLE_H: i32 = 34;

/// A window at `(x, y)` (its own area; the title bar above it) of `size`,
/// mapped and managed, closing on `WM_DELETE_WINDOW` (Helix's close button
/// kills a client without it).
fn window_at(x: &X, (wx, wy): (i32, i32), (w, h): (u16, u16)) -> Window {
    let window = x.create((wx as i16, wy as i16), (w, h));
    x.set_atoms(window, "WM_PROTOCOLS", &["WM_DELETE_WINDOW"]);
    x.map_managed(window);
    x.conn.configure_window(window, &ConfigureWindowAux::new().x(wx).y(wy).width(u32::from(w)).height(u32::from(h))).unwrap();
    x.flush();
    until("the window did not go where it asked", || x.position(window) == (wx as i16, wy as i16) && x.size(window) == (w, h));
    x.sync();
    window
}

/// Puts a finger down at `from`, moves it by `by` in ten steps, lifts it.
fn drag(session: &mut HeadlessSession, (fx, fy): (i32, i32), (dx, dy): (i32, i32)) {
    session.input(&format!("touch down {fx} {fy}"));
    for step in 1..=10 {
        session.input(&format!("touch move {} {}", fx + step * dx / 10, fy + step * dy / 10));
    }
    session.input("touch up");
}

fn tap(session: &mut HeadlessSession, (x, y): (i32, i32)) {
    session.input(&format!("touch down {x} {y}"));
    session.input("touch up");
}

/// Whether `window` is asked to close (`WM_DELETE_WINDOW`) within `time`.
fn asked_to_close(x: &X, window: Window, time: Duration) -> bool {
    let protocols = x.atom("WM_PROTOCOLS");
    let delete = x.atom("WM_DELETE_WINDOW");
    x.events_for(time).iter().any(|e| matches!(e, Event::ClientMessage(m) if m.window == window && m.type_ == protocols && m.data.as_data32()[0] == delete))
}

/// The middle of the close button of a `w`-wide window at `(wx, wy)`
/// (`appmenu:close` by default: the right end of the title bar, 4 pixels
/// in, 30 wide).
fn close_button((wx, wy): (i32, i32), w: i32) -> (i32, i32) {
    (wx + w - 4 - 15, wy - TITLE_H / 2)
}

/// X140: a finger on the title bar moves the window, which gets focus;
/// the window behind the title bar does not.
#[test]
fn a_finger_on_the_title_bar_moves_the_window() {
    let Some(mut session) = xwm::session() else { return };
    let x = X::new(&session);
    let behind = window_at(&x, (100, 100), (400, 300));
    let framed = window_at(&x, (150, 300), (200, 100));
    let other = window_at(&x, (560, 460), (200, 100));
    x.focus(other);
    // On the title bar, over the window behind.
    drag(&mut session, (250, 300 - TITLE_H / 2), (100, -50));
    until("the window did not follow the finger on its title bar", || x.position(framed) == (250, 250));
    until("the window moved by its title bar did not get focus", || x.active() == Some(framed));
    x.sync();
    assert_eq!(x.active(), Some(framed), "the window behind the title bar took the focus");
    assert_ne!(x.input_focus(), behind, "the window behind the title bar got the X focus");
}

/// X140: a tap on the title bar of a window over another gives the
/// framed window focus, not the one behind its title bar.
#[test]
fn a_tap_on_the_title_bar_does_not_focus_the_window_behind() {
    let Some(mut session) = xwm::session() else { return };
    let x = X::new(&session);
    let behind = window_at(&x, (100, 100), (400, 300));
    let framed = window_at(&x, (150, 300), (200, 100));
    let other = window_at(&x, (560, 460), (200, 100));
    x.focus(other);
    tap(&mut session, (250, 300 - TITLE_H / 2));
    x.sync();
    until("nothing got focus from the tap on the title bar", || x.active() != Some(other));
    assert_ne!(x.active(), Some(behind), "the window behind the title bar took the focus");
    assert_eq!(x.active(), Some(framed), "the tapped window did not get focus");
}

/// X140: a finger on the bottom-right border resizes the window; the top
/// left stays put.
#[test]
fn a_finger_on_the_border_resizes_the_window() {
    let Some(mut session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = window_at(&x, (100, 100), (300, 200));
    // The border reaches 10 pixels out (`draggable-border-width`).
    drag(&mut session, (100 + 300 + 4, 100 + 200 + 4), (60, 40));
    until("the bottom right corner did not follow the finger", || x.size(window) == (360, 240));
    assert_eq!(x.position(window), (100, 100), "the top left corner moved");
}

/// X140: a tap on the close button closes the window; a finger that went
/// down on it and slid off before lifting does not.
#[test]
fn a_tap_on_the_close_button_closes_the_window() {
    let Some(mut session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = window_at(&x, (100, 100), (300, 200));
    let (cx, cy) = close_button((100, 100), 300);
    x.drain();
    // Slid off onto the title.
    drag(&mut session, (cx, cy), (-150, 0));
    assert!(!asked_to_close(&x, window, Duration::from_millis(500)), "a finger that slid off the close button closed the window");
    assert_eq!(x.position(window), (100, 100), "a finger that went down on the close button moved the window");
    tap(&mut session, (cx, cy));
    assert!(asked_to_close(&x, window, Duration::from_secs(5)), "a tap on the close button did not close the window");
}

/// X140: a double tap on the title bar does `action-double-click-titlebar`
/// (toggle maximize by default).
#[test]
fn a_double_tap_on_the_title_bar_maximizes() {
    // A long double-click time: the two taps are separate commands.
    let Some(mut session) = xwm::session_settings_env("[org/gnome/desktop/peripherals/mouse]\ndouble-click=3000\n", Vec::new()) else { return };
    let x = X::new(&session);
    let window = window_at(&x, (100, 100), (300, 200));
    tap(&mut session, (200, 100 - TITLE_H / 2));
    tap(&mut session, (203, 100 - TITLE_H / 2));
    until("a double tap on the title bar did not maximize the window", || x.has_state(window, "_NET_WM_STATE_MAXIMIZED_VERT"));
    until("the maximized window is not as wide as the screen", || x.size(window).0 == xwm::SCREEN.0);
}
