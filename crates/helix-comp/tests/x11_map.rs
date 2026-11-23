//! X11 windows shown, hidden and taken in by the window manager: the edge
//! cases of `tests/x11_cases.md` about mapping, withdrawing, `WM_STATE`,
//! override-redirect windows and which windows are managed at all (#191).
//!
//! Runs the real `helix-comp` headless; needs `dbus-daemon` and Xwayland
//! (as every headless session), skipped otherwise.

mod common;

use std::time::Duration;

use common::xwm::{self, until, X};
use x11rb::properties::{WmHints, WmHintsState};
use x11rb::protocol::xproto::{AtomEnum, ChangeWindowAttributesAux, ConfigureWindowAux, ConnectionExt, CreateWindowAux, EventMask, MapState, Window, WindowClass};
use x11rb::connection::Connection;
use x11rb::protocol::Event;

fn viewable(x: &X, window: Window) -> bool {
    x.conn.get_window_attributes(window).unwrap().reply().unwrap().map_state == MapState::VIEWABLE
}

/// X1: shown and hidden over and over before the window manager catches
/// up (Zoom's toolbar): it ends shown, managed, and nothing loops.
#[test]
fn withdraw_and_map_in_a_row_ends_shown() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.window();
    x.map(window);
    x.withdraw(window);
    x.map(window);
    x.withdraw(window);
    x.map(window);
    until("the window shown last is not managed", || x.managed(window) && viewable(&x, window));
    std::thread::sleep(Duration::from_millis(300));
    x.drain();
    let events = x.events_for(Duration::from_secs(1));
    assert!(events.len() < 10, "{} events in a second after the last map", events.len());
    assert!(viewable(&x, window) && x.managed(window), "the window did not stay shown");
}

/// X1: an override-redirect window's synthetic UnmapNotify is not a
/// request to the window manager: the menu stays shown.
#[test]
fn a_synthetic_unmap_of_a_menu_is_ignored() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let menu = x.popup((300, 300), (100, 100));
    until("the menu is not shown", || viewable(&x, menu));
    x.synthetic_unmap(menu);
    std::thread::sleep(Duration::from_millis(300));
    assert!(viewable(&x, menu), "the menu was hidden");
    xwm::wm_still_works(&session);
}

/// X3: mapped three times without unmapping (GTK 4 apps do): listed once.
#[test]
fn a_window_mapped_three_times_is_listed_once() {
    if common::known_failure(&[common::Xwm::Smithay], "a window mapped again while mapped is listed twice in _NET_CLIENT_LIST_STACKING (#191)") {
        return;
    }
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.window();
    x.map(window);
    x.map(window);
    x.map_managed(window);
    until("the window is not listed", || x.client_list().contains(&window));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(x.client_list().iter().filter(|w| **w == window).count(), 1, "_NET_CLIENT_LIST: {:?}", x.client_list());
    assert_eq!(x.stacking_list().iter().filter(|w| **w == window).count(), 1, "_NET_CLIENT_LIST_STACKING: {:?}", x.stacking_list());
    x.conn.destroy_window(window).unwrap();
    x.flush();
    until("the destroyed window is still listed", || !x.client_list().contains(&window) && !x.stacking_list().contains(&window));
}

/// X4: `WM_STATE` is Normal while shown, Withdrawn once hidden, Normal
/// again shown again, always as a `WM_STATE`-typed pair (Wine waits for
/// Withdrawn; toolkits read it to know they are managed).
#[test]
fn wm_state_follows_the_window() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.mapped();
    let wm_state = x.atom("WM_STATE");
    let reply = x.conn.get_property(false, window, wm_state, AtomEnum::ANY, 0, 2).unwrap().reply().unwrap();
    assert_eq!((reply.type_, reply.format), (wm_state, 32), "WM_STATE's type and format");
    assert_eq!(reply.value32().unwrap().collect::<Vec<_>>(), vec![1, 0], "Normal, no icon window");
    x.withdraw(window);
    until("WM_STATE is not Withdrawn after hiding", || x.wm_state(window) == Some(0));
    x.map_managed(window);
}

/// X4: EWMH: the window manager removes `_NET_WM_STATE` from a withdrawn
/// window (the app starts afresh when it maps it again).
#[test]
fn net_wm_state_is_removed_when_withdrawn() {
    if common::known_failure(&[common::Xwm::Smithay], "_NET_WM_STATE stays on a withdrawn window (#191)") {
        return;
    }
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.mapped();
    x.net_wm_state(window, 1, &["_NET_WM_STATE_ABOVE"]);
    until("Always on Top was not set", || x.has_state(window, "_NET_WM_STATE_ABOVE"));
    x.withdraw(window);
    until("WM_STATE is not Withdrawn", || x.wm_state(window) == Some(0));
    until("_NET_WM_STATE is left on the withdrawn window", || x.get32(window, "_NET_WM_STATE").is_none());
}

/// X5: `WM_HINTS` asks to start iconic (XCOM 2): the window starts
/// minimized, `WM_STATE` Iconic and `_NET_WM_STATE_HIDDEN`.
#[test]
fn a_window_asking_to_start_iconic_starts_minimized() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.window();
    let mut hints = WmHints::new();
    hints.initial_state = Some(WmHintsState::Iconic);
    hints.set(&x.conn, window).unwrap();
    x.map(window);
    until("the window is not Iconic", || x.wm_state(window) == Some(3));
    assert!(x.has_state(window, "_NET_WM_STATE_HIDDEN"), "_NET_WM_STATE_HIDDEN is not set");
}

/// X6, X7: minimized by the app (`WM_CHANGE_STATE` Iconic): Iconic and
/// HIDDEN; restored: Normal, no HIDDEN. Asking for Normal does not
/// minimize (Dishonored 2 restores this way).
#[test]
fn minimize_and_restore() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.mapped();
    x.tell_wm(window, "WM_CHANGE_STATE", [1, 0, 0, 0, 0]);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(x.wm_state(window), Some(1), "asking for Normal minimized the window");
    x.tell_wm(window, "WM_CHANGE_STATE", [3, 0, 0, 0, 0]);
    until("the minimized window is not Iconic", || x.wm_state(window) == Some(3));
    assert!(x.has_state(window, "_NET_WM_STATE_HIDDEN"), "_NET_WM_STATE_HIDDEN is not set");
    x.activate(window, 2, x11rb::NONE);
    until("the restored window is not Normal", || x.wm_state(window) == Some(1));
    assert!(!x.has_state(window, "_NET_WM_STATE_HIDDEN"), "_NET_WM_STATE_HIDDEN stayed");
}

/// X6: focus and fullscreen changes leave `WM_STATE` alone: Steam reads
/// a rewritten `WM_STATE` as its UI being active.
#[test]
fn focus_and_fullscreen_leave_wm_state_alone() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let a = x.mapped();
    let b = x.mapped();
    x.focus(b);
    std::thread::sleep(Duration::from_millis(200));
    x.drain();
    for _ in 0..3 {
        x.focus(a);
        x.focus(b);
    }
    x.net_wm_state(b, 1, &["_NET_WM_STATE_FULLSCREEN"]);
    until("the window did not go fullscreen", || x.size(b) == xwm::SCREEN);
    x.net_wm_state(b, 0, &["_NET_WM_STATE_FULLSCREEN"]);
    until("the window stayed fullscreen", || x.size(b) != xwm::SCREEN);
    let wm_state = x.atom("WM_STATE");
    let rewrites = x.events_for(Duration::from_millis(300)).iter().filter(|e| matches!(e, Event::PropertyNotify(p) if p.atom == wm_state)).count();
    assert_eq!(rewrites, 0, "WM_STATE was rewritten on focus or fullscreen changes");
}

/// X8: override-redirect cleared before mapping (REAPER's and GIMP's
/// menus, the Marvel Rivals launcher): a normal, managed window.
#[test]
fn override_redirect_cleared_before_mapping_is_managed() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.create_with(10, 10, 200, 100, CreateWindowAux::new().override_redirect(1));
    x.conn.change_window_attributes(window, &ChangeWindowAttributesAux::new().override_redirect(0)).unwrap();
    x.map(window);
    until("the window is not managed", || x.managed(window));
    until("the window is not listed", || x.client_list().contains(&window));
    until("the window is not shown", || viewable(&x, window));
}

/// X8: override-redirect set before mapping: left alone, not listed.
#[test]
fn override_redirect_set_before_mapping_is_left_alone() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.window();
    x.conn.change_window_attributes(window, &ChangeWindowAttributesAux::new().override_redirect(1)).unwrap();
    x.map(window);
    until("the menu is not shown", || viewable(&x, window));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(x.wm_state(window), None, "the window manager took an override-redirect window in");
    assert!(!x.client_list().contains(&window), "an override-redirect window is listed");
}

/// X9: a window that is a normal window first and an override-redirect
/// one the next time it is shown (VLC), and back.
#[test]
fn override_redirect_changed_between_maps() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.mapped();
    x.withdraw(window);
    until("the window was not withdrawn", || x.wm_state(window) == Some(0));
    x.conn.change_window_attributes(window, &ChangeWindowAttributesAux::new().override_redirect(1)).unwrap();
    x.map(window);
    until("the window is not shown", || viewable(&x, window));
    std::thread::sleep(Duration::from_millis(300));
    assert!(!x.client_list().contains(&window), "the override-redirect window is still listed");
    x.conn.unmap_window(window).unwrap();
    x.conn.change_window_attributes(window, &ChangeWindowAttributesAux::new().override_redirect(0)).unwrap();
    x.map_managed(window);
    until("the managed window is not listed again", || x.client_list().contains(&window));
}

/// X10: the window manager never configures, lists or focuses
/// override-redirect windows (GIMP's submenus broke).
#[test]
fn override_redirect_windows_are_left_alone() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let a = x.mapped();
    let menu = x.popup((300, 300), (100, 100));
    until("the menu is not shown", || viewable(&x, menu));
    let b = x.mapped();
    std::thread::sleep(Duration::from_millis(200));
    x.drain();
    x.focus(a);
    x.focus(b);
    x.activate(menu, 2, x11rb::NONE);
    std::thread::sleep(Duration::from_millis(300));
    let configured = x.events_for(Duration::from_millis(200)).iter().any(|e| matches!(e, Event::ConfigureNotify(c) if c.window == menu));
    assert!(!configured, "the window manager configured the menu");
    assert!(!x.client_list().contains(&menu) && !x.stacking_list().contains(&menu), "the menu is listed");
    assert_ne!(x.active(), Some(menu), "the menu became the active window");
    assert_eq!(x.position(menu), (300, 300), "the menu moved");
}

/// X11: an override-redirect window that moves itself gets the pointer
/// where it is now.
#[test]
fn a_menu_that_moves_gets_the_pointer_where_it_is() {
    let Some(mut session) = xwm::session() else { return };
    let x = X::new(&session);
    let menu = x.create_with(100, 100, 80, 80, CreateWindowAux::new().override_redirect(1));
    x.conn.change_window_attributes(menu, &ChangeWindowAttributesAux::new().event_mask(EventMask::ENTER_WINDOW | EventMask::STRUCTURE_NOTIFY)).unwrap();
    x.map(menu);
    until("the menu is not shown", || viewable(&x, menu));
    let entered = |x: &X| x.wait_event("the pointer never entered the menu", |e| matches!(e, Event::EnterNotify(n) if n.event == menu).then_some(()));
    std::thread::sleep(Duration::from_millis(200));
    session.input("move 140 140");
    entered(&x);
    x.conn.configure_window(menu, &ConfigureWindowAux::new().x(500).y(300)).unwrap();
    x.flush();
    until("the menu did not move", || x.position(menu) == (500, 300));
    session.input("move 20 500");
    std::thread::sleep(Duration::from_millis(200));
    x.drain();
    session.input("move 540 340");
    entered(&x);
}

/// X12: an override-redirect window shown, hidden and shown again (GTK
/// reuses one for each popup menu): it works every time.
#[test]
fn a_menu_shown_again_works_again() {
    let Some(mut session) = xwm::session() else { return };
    let x = X::new(&session);
    let menu = x.create_with(300, 300, 100, 100, CreateWindowAux::new().override_redirect(1));
    x.conn.change_window_attributes(menu, &ChangeWindowAttributesAux::new().event_mask(EventMask::ENTER_WINDOW | EventMask::POINTER_MOTION | EventMask::STRUCTURE_NOTIFY)).unwrap();
    for round in 0..3 {
        session.input("move 20 20");
        x.map(menu);
        until("the menu is not shown", || viewable(&x, menu));
        // Time for the menu's surface to take its place on the screen.
        std::thread::sleep(Duration::from_millis(1000));
        x.drain();
        // The pointer comes in and moves over the menu: the menu gets its
        // motion. (Not EnterNotify: X still has the pointer where it last
        // saw it, over the menu shown before, so it may enter the menu as
        // soon as it is mapped there, before the pointer really comes.)
        session.input("move 350 350");
        session.input("move 360 360");
        x.wait_event(&format!("the pointer never reached the menu shown again ({round})"), |e| matches!(e, Event::MotionNotify(n) if n.event == menu).then_some(()));
        x.conn.unmap_window(menu).unwrap();
        x.flush();
        until("the menu is still shown", || !viewable(&x, menu));
    }
}

/// X13: InputOnly windows (Chromium's drag helpers) are never managed.
#[test]
fn input_only_windows_are_not_managed() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.conn.generate_id().unwrap();
    x.conn.create_window(0, window, x.root, 10, 10, 50, 50, 0, WindowClass::INPUT_ONLY, 0, &CreateWindowAux::new()).unwrap();
    x.map(window);
    let plain = x.mapped();
    std::thread::sleep(Duration::from_millis(200));
    assert!(x.client_list().contains(&plain));
    assert!(!x.client_list().contains(&window), "an InputOnly window is listed");
    assert_ne!(x.wm_state(window), Some(1), "an InputOnly window is managed");
}

/// X14: the window manager's own windows (its check window, clipboard
/// windows) are never listed as apps' windows.
#[test]
fn the_window_managers_own_windows_are_not_listed() {
    let Some(session) = xwm::session() else { return };
    let mut app = common::connect(&session);
    let (wayland, ()) = app.toplevel((200, 100), |_, _| {});
    let surface = wayland.surface.clone();
    app.until("the Wayland window has no focus", |s| s.keyboard.as_ref() == Some(&surface));
    app.copy("text/plain;charset=utf-8", b"hello");
    let x = X::new(&session);
    let ours = [x.mapped(), x.mapped()];
    std::thread::sleep(Duration::from_millis(300));
    let listed = x.client_list();
    assert!(listed.iter().all(|w| ours.contains(w)), "windows not ours are listed: {listed:?} (ours {ours:?})");
    let check = x.get32(x.root, "_NET_SUPPORTING_WM_CHECK").and_then(|v| v.first().copied());
    assert!(check.is_some_and(|c| !listed.contains(&c)));
}

/// X15: `_NET_CLIENT_LIST` holds managed windows in the order they were
/// mapped, and only while they are.
#[test]
fn the_client_list_is_in_map_order() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let a = x.window();
    let b = x.window();
    x.map_managed(b);
    x.map_managed(a);
    until("the windows are not listed", || x.client_list().len() == 2);
    assert_eq!(x.client_list(), vec![b, a], "_NET_CLIENT_LIST is not in map order");
    x.withdraw(a);
    until("the hidden window is still listed", || x.client_list() == vec![b]);
}

/// X16: windows destroyed as soon as they are mapped, many times: the X
/// server is never left grabbed and the window manager keeps working.
#[test]
fn windows_destroyed_right_after_mapping() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    for _ in 0..50 {
        let window = x.window();
        x.conn.map_window(window).unwrap();
        x.conn.destroy_window(window).unwrap();
    }
    x.flush();
    let observer = X::new(&session);
    for _ in 0..10 {
        assert!(observer.answers(), "the X server does not answer (a server grab left behind?)");
        std::thread::sleep(Duration::from_millis(50));
    }
    xwm::wm_still_works(&session);
}

/// X17: shown and hidden at once, then shown again (GeoGebra): managed
/// once, and it takes focus.
#[test]
fn mapped_unmapped_and_mapped_again_at_once() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.window();
    x.conn.map_window(window).unwrap();
    x.conn.unmap_window(window).unwrap();
    x.conn.map_window(window).unwrap();
    x.flush();
    until("the window is not managed", || x.managed(window) && viewable(&x, window));
    until("the window never got focus", || x.active() == Some(window));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(x.client_list().iter().filter(|w| **w == window).count(), 1, "_NET_CLIENT_LIST: {:?}", x.client_list());
}

/// X18: forged or repeated `WL_SURFACE_ID` and `WL_SURFACE_SERIAL`
/// messages from any client: ignored, windows keep working.
#[test]
fn forged_surface_messages_are_ignored() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.mapped();
    until("the window has no focus", || x.active() == Some(window));
    for bogus in [0, 1, 2, 0xffff_ffff, window] {
        x.tell_wm(window, "WL_SURFACE_ID", [bogus, 0, 0, 0, 0]);
        x.tell_wm(window, "WL_SURFACE_SERIAL", [bogus, 0, 0, 0, 0]);
        x.tell_wm(0x0dea_dbee, "WL_SURFACE_ID", [bogus, 0, 0, 0, 0]);
    }
    std::thread::sleep(Duration::from_millis(300));
    let other = x.mapped();
    x.focus(window);
    x.focus(other);
    xwm::wm_still_works(&session);
}

/// X19: a parent destroyed before its dialogs (closing GIMP): the dialogs
/// stay managed and can take focus.
#[test]
fn a_parent_destroyed_before_its_dialogs() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let parent = x.mapped();
    let dialogs: Vec<Window> = (0..2)
        .map(|_| {
            let dialog = x.window();
            x.set32(dialog, "WM_TRANSIENT_FOR", AtomEnum::WINDOW, &[parent]);
            x.map_managed(dialog);
            dialog
        })
        .collect();
    x.conn.destroy_window(parent).unwrap();
    x.flush();
    until("the destroyed parent is still listed", || !x.client_list().contains(&parent));
    x.focus(dialogs[0]);
    x.focus(dialogs[1]);
    for dialog in dialogs {
        x.conn.destroy_window(dialog).unwrap();
    }
    x.flush();
    xwm::wm_still_works(&session);
}

/// X20: hundreds of windows (PySolFC): all listed, all gone once
/// destroyed.
#[test]
fn hundreds_of_windows() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let windows: Vec<Window> = (0..300).map(|_| x.window()).collect();
    for &w in &windows {
        x.conn.map_window(w).unwrap();
    }
    x.flush();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let listed = x.client_list();
        let missing = windows.iter().filter(|w| !listed.contains(w)).count();
        if missing == 0 {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{missing} of {} windows are not listed", windows.len());
        std::thread::sleep(Duration::from_millis(100));
    }
    for &w in &windows {
        x.conn.destroy_window(w).unwrap();
    }
    x.flush();
    until("destroyed windows are still listed", || x.client_list().is_empty());
    xwm::wm_still_works(&session);
}
