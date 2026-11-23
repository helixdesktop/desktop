//! What Helix's own X11 window manager tells X11 apps about the desktop
//! (helixdesktop/desktop#227): the work area (Qt's `availableGeometry`,
//! Java's screen insets, Wine's work area), the workspaces and the one each
//! window is on, the actions it allows, `_NET_FRAME_EXTENTS` asked for
//! before mapping (`_NET_REQUEST_FRAME_EXTENTS`, Qt, GTK and Java) and kept
//! right afterwards, and `_NET_WM_MOVERESIZE` with the keyboard and cancel.
//!
//! Helix's window manager only (`HELIX_XWM=helix`); skipped under
//! Smithay's. Needs `dbus-daemon` and Xwayland; skipped otherwise.

mod common;

use std::time::Duration;

use common::xwm::{self, until, X};

/// Helix's title bar height (`helix_comp::frames::TITLE_H`).
const TITLE_H: u32 = 34;

fn helix() -> bool {
    if common::xwm_running() != common::Xwm::Helix {
        eprintln!("Helix's own window manager only (HELIX_XWM=helix); skipped");
        return false;
    }
    true
}

fn root32(x: &X, property: &str) -> Vec<u32> {
    x.get32(x.root, property).unwrap_or_default()
}

/// X81-adjacent (EWMH root properties): the ones apps read are announced
/// and set, consistently with each other.
#[test]
fn the_desktop_is_described_to_apps() {
    if !helix() {
        return;
    }
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let supported = root32(&x, "_NET_SUPPORTED");
    for name in ["_NET_WORKAREA", "_NET_NUMBER_OF_DESKTOPS", "_NET_CURRENT_DESKTOP", "_NET_WM_DESKTOP", "_NET_DESKTOP_GEOMETRY", "_NET_REQUEST_FRAME_EXTENTS", "_NET_WM_ALLOWED_ACTIONS", "_NET_WM_ACTION_CLOSE"] {
        assert!(supported.contains(&x.atom(name)), "{name} is not in _NET_SUPPORTED");
    }
    let count = root32(&x, "_NET_NUMBER_OF_DESKTOPS");
    assert_eq!(count.len(), 1);
    assert!(count[0] >= 1);
    let current = root32(&x, "_NET_CURRENT_DESKTOP");
    assert!(current.len() == 1 && current[0] < count[0], "{current:?} of {count:?}");
    let area = root32(&x, "_NET_WORKAREA");
    assert_eq!(area.len(), 4 * count[0] as usize, "one work area per desktop");
    assert_eq!(&area[..4], &[0, 0, u32::from(xwm::SCREEN.0), u32::from(xwm::SCREEN.1)], "no panels: the whole screen");
    assert_eq!(root32(&x, "_NET_DESKTOP_GEOMETRY"), [u32::from(xwm::SCREEN.0), u32::from(xwm::SCREEN.1)]);
    // A managed window: the actions Helix allows, and its workspace.
    let window = x.mapped();
    until("the window has no _NET_WM_ALLOWED_ACTIONS", || x.get32(window, "_NET_WM_ALLOWED_ACTIONS").is_some_and(|a| a.contains(&x.atom("_NET_WM_ACTION_MOVE")) && a.contains(&x.atom("_NET_WM_ACTION_CLOSE"))));
    until("the window has no _NET_WM_DESKTOP", || x.get32(window, "_NET_WM_DESKTOP") == Some(vec![root32(&x, "_NET_CURRENT_DESKTOP")[0]]));
}

/// The work area leaves out a panel's exclusive zone (Qt and Java
/// maximize themselves into it; Wine's work area), here and per monitor
/// (`_GTK_WORKAREAS_D0`), and comes back when the panel goes.
#[test]
fn the_work_area_follows_the_panels() {
    if !helix() {
        return;
    }
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let (w, h) = (u32::from(xwm::SCREEN.0), u32::from(xwm::SCREEN.1));
    let mut app = common::connect(&session);
    let panel = app.panel(40);
    until("the work area doesn't leave out the panel", || root32(&x, "_NET_WORKAREA").get(..4) == Some(&[0, 40, w, h - 40][..]));
    assert_eq!(root32(&x, "_GTK_WORKAREAS_D0"), [0, 40, w, h - 40], "the monitor's work area");
    panel.layer.destroy();
    panel.surface.destroy();
    app.roundtrip();
    until("the work area didn't come back without the panel", || root32(&x, "_NET_WORKAREA").get(..4) == Some(&[0, 0, w, h][..]));
}

/// `_NET_REQUEST_FRAME_EXTENTS` (Qt, GTK and Java ask before mapping, and
/// Java waits): answered with the title bar Helix will draw, or none for a
/// window whose Motif hints ask for no decorations.
#[test]
fn frame_extents_are_answered_before_mapping() {
    if !helix() {
        return;
    }
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let framed = x.window();
    x.tell_wm(framed, "_NET_REQUEST_FRAME_EXTENTS", [0; 5]);
    until("no _NET_FRAME_EXTENTS for the request", || x.get32(framed, "_NET_FRAME_EXTENTS") == Some(vec![0, 0, TITLE_H, 0]));
    let bare = x.window();
    // _MOTIF_WM_HINTS: flags (decorations given), functions, decorations 0.
    let motif = x.atom("_MOTIF_WM_HINTS");
    x.set32(bare, "_MOTIF_WM_HINTS", motif, &[2, 0, 0, 0, 0]);
    x.tell_wm(bare, "_NET_REQUEST_FRAME_EXTENTS", [0; 5]);
    until("no _NET_FRAME_EXTENTS for the undecorated window", || x.get32(bare, "_NET_FRAME_EXTENTS") == Some(vec![0, 0, 0, 0]));
}

/// `_NET_FRAME_EXTENTS` stays true after mapping: none while full screen
/// (no title bar then), back after, and none once the app's Motif hints
/// ask for no decorations.
#[test]
fn frame_extents_follow_fullscreen_and_motif_hints() {
    if !helix() {
        return;
    }
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.mapped();
    until("no _NET_FRAME_EXTENTS when shown", || x.get32(window, "_NET_FRAME_EXTENTS") == Some(vec![0, 0, TITLE_H, 0]));
    x.net_wm_state(window, 1, &["_NET_WM_STATE_FULLSCREEN"]);
    until("the full-screen window keeps a title bar in _NET_FRAME_EXTENTS", || x.get32(window, "_NET_FRAME_EXTENTS") == Some(vec![0, 0, 0, 0]));
    x.net_wm_state(window, 0, &["_NET_WM_STATE_FULLSCREEN"]);
    until("the title bar didn't come back in _NET_FRAME_EXTENTS", || x.get32(window, "_NET_FRAME_EXTENTS") == Some(vec![0, 0, TITLE_H, 0]));
    let motif = x.atom("_MOTIF_WM_HINTS");
    x.set32(window, "_MOTIF_WM_HINTS", motif, &[2, 0, 0, 0, 0]);
    until("_NET_FRAME_EXTENTS kept the title bar after the Motif hints asked for none", || x.get32(window, "_NET_FRAME_EXTENTS") == Some(vec![0, 0, 0, 0]));
}

/// `_NET_WM_MOVERESIZE` 10 (move with the keyboard) moves the window with
/// the arrow keys, and 11 (cancel) ends it, putting it back.
#[test]
fn keyboard_move_and_cancel() {
    if !helix() {
        return;
    }
    let Some(mut session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.mapped();
    x.focus(window);
    std::thread::sleep(Duration::from_millis(200));
    let start = x.position(window);
    let moveresize = |direction: u32| x.tell_wm(window, "_NET_WM_MOVERESIZE", [0, 0, direction, 0, 1]);
    moveresize(10);
    x.flush();
    // Right arrow (evdev 106): 10 px each.
    let moved = || {
        let p = x.position(window);
        p.0 > start.0
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !moved() {
        assert!(std::time::Instant::now() < deadline, "the arrow keys didn't move the window after a keyboard _NET_WM_MOVERESIZE");
        session.input("key 106 1");
        session.input("key 106 0");
        std::thread::sleep(Duration::from_millis(100));
    }
    moveresize(11);
    x.flush();
    until("the cancel didn't put the window back", || x.position(window) == start);
    // The move is over: the arrow keys no longer move it.
    session.input("key 106 1");
    session.input("key 106 0");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(x.position(window), start, "the arrow keys still move the window after the cancel");
}
