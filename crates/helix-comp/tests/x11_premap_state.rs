//! `_NET_WM_STATE` asked for by client message before the window is
//! mapped (#276). EWMH has a client set the property itself before
//! mapping, but some (SDL and Wine paths) send the message instead, or
//! both: as mutter, what the messages asked for counts on top of the
//! property read when the window is managed, not replaced by it.

mod common;

use common::xwm::{session, until, X, SCREEN};
use common::Xwm;

const WHY: &str = "Smithay's window manager drops _NET_WM_STATE messages sent before the window is mapped (#276)";

/// Wait until `window` covers the screen and says it is full screen.
fn until_full_screen(x: &X, window: u32) {
    until("the window did not map full screen", || x.has_state(window, "_NET_WM_STATE_FULLSCREEN"));
    until("the full-screen window doesn't cover the screen", || x.position(window) == (0, 0) && x.size(window) == SCREEN);
}

/// Full screen asked for by message only, before mapping: it maps full
/// screen.
#[test]
fn full_screen_asked_by_message_before_mapping() {
    if common::known_failure(&[Xwm::Smithay], WHY) {
        return;
    }
    let Some(session) = session() else { return };
    let x = X::new(&session);
    let window = x.window();
    x.net_wm_state(window, 1, &["_NET_WM_STATE_FULLSCREEN"]);
    x.map_managed(window);
    until_full_screen(&x, window);
}

/// Maximized asked for by message only, before mapping: it maps
/// maximized.
#[test]
fn maximized_asked_by_message_before_mapping() {
    if common::known_failure(&[Xwm::Smithay], WHY) {
        return;
    }
    let Some(session) = session() else { return };
    let x = X::new(&session);
    let window = x.window();
    x.net_wm_state(window, 1, &["_NET_WM_STATE_MAXIMIZED_HORZ", "_NET_WM_STATE_MAXIMIZED_VERT"]);
    x.map_managed(window);
    until("the window did not map maximized", || x.has_state(window, "_NET_WM_STATE_MAXIMIZED_HORZ") && x.has_state(window, "_NET_WM_STATE_MAXIMIZED_VERT"));
    until("the maximized window isn't as wide as the screen", || x.size(window).0 == SCREEN.0);
}

/// The property set and a message adding another state: both count.
#[test]
fn the_property_and_a_message_before_mapping_both_count() {
    if common::known_failure(&[Xwm::Smithay], WHY) {
        return;
    }
    let Some(session) = session() else { return };
    let x = X::new(&session);
    let window = x.window();
    x.set_atoms(window, "_NET_WM_STATE", &["_NET_WM_STATE_ABOVE"]);
    x.net_wm_state(window, 1, &["_NET_WM_STATE_FULLSCREEN"]);
    x.map_managed(window);
    until_full_screen(&x, window);
    until("the state the property set was lost", || x.has_state(window, "_NET_WM_STATE_ABOVE"));
}

/// A message removing a state the property sets: the message, the later
/// word, counts.
#[test]
fn a_message_before_mapping_removes_a_state_the_property_sets() {
    if common::known_failure(&[Xwm::Smithay], WHY) {
        return;
    }
    let Some(session) = session() else { return };
    let x = X::new(&session);
    let window = x.window();
    x.set_atoms(window, "_NET_WM_STATE", &["_NET_WM_STATE_FULLSCREEN", "_NET_WM_STATE_ABOVE"]);
    x.net_wm_state(window, 0, &["_NET_WM_STATE_FULLSCREEN"]);
    x.map_managed(window);
    until("the state the property set and no message removed was lost", || x.has_state(window, "_NET_WM_STATE_ABOVE"));
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!x.has_state(window, "_NET_WM_STATE_FULLSCREEN"), "full screen the message removed was applied");
    assert_ne!(x.size(window), SCREEN, "the window maps full screen though the message removed it");
}

/// Asking for its frame extents before mapping (GTK, Qt, Java do) reads
/// the window's properties early: the state a message asked for before
/// that still counts.
#[test]
fn a_frame_extents_request_before_mapping_keeps_the_state_asked_for() {
    if common::known_failure(&[Xwm::Smithay], WHY) {
        return;
    }
    let Some(session) = session() else { return };
    let x = X::new(&session);
    let window = x.window();
    x.net_wm_state(window, 1, &["_NET_WM_STATE_FULLSCREEN"]);
    x.tell_wm(window, "_NET_REQUEST_FRAME_EXTENTS", [0; 5]);
    until("_NET_FRAME_EXTENTS was not set before mapping", || x.get32(window, "_NET_FRAME_EXTENTS").is_some());
    x.map_managed(window);
    until_full_screen(&x, window);
}
