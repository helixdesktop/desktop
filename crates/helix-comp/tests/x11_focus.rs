//! Keyboard focus for X11 windows, as the apps see it: the edge cases of
//! `tests/x11_cases.md` about ICCCM input models, `_NET_ACTIVE_WINDOW`,
//! focus events the window manager must not act on, keyboard grabs,
//! `_NET_WM_STATE_FOCUSED` and pings (#191).
//!
//! Runs the real `helix-comp` headless; needs `dbus-daemon` and Xwayland
//! (as every headless session), skipped otherwise.

mod common;

use std::time::{Duration, Instant};

use common::xwm::{self, until, X};
use x11rb::connection::Connection;
use x11rb::properties::WmHints;
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConnectionExt, CreateWindowAux, EventMask, GrabMode, GrabStatus, InputFocus, NotifyDetail, Window, WindowClass,
};
use x11rb::protocol::Event;

/// A window with `WM_HINTS` input `input` (none at all for `None`) and
/// `WM_PROTOCOLS` `protocols`, mapped.
fn window_with(x: &X, input: Option<bool>, protocols: &[&str]) -> Window {
    let window = x.window();
    if let Some(input) = input {
        let mut hints = WmHints::new();
        hints.input = Some(input);
        hints.set(&x.conn, window).unwrap();
    }
    if !protocols.is_empty() {
        x.set_atoms(window, "WM_PROTOCOLS", protocols);
    }
    x.map(window);
    window
}

/// Whether `window` got `WM_PROTOCOLS` `protocol` in the next `time`.
fn got_protocol(x: &X, window: Window, protocol: &str, time: Duration) -> bool {
    let (wm_protocols, protocol) = (x.atom("WM_PROTOCOLS"), x.atom(protocol));
    x.events_for(time).iter().any(|e| matches!(e, Event::ClientMessage(m) if m.window == window && m.type_ == wm_protocols && m.data.as_data32()[0] == protocol))
}

/// X21: Passive (input, no `WM_TAKE_FOCUS`): the window manager gives it
/// the X focus, and sends nothing (xterm closes on unexpected protocol
/// messages).
#[test]
fn passive_windows_get_the_input_focus() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = window_with(&x, Some(true), &[]);
    until("the window never got the X focus", || x.input_focus() == window);
    let wm_protocols = x.atom("WM_PROTOCOLS");
    let messages = x.events_for(Duration::from_millis(500)).iter().filter(|e| matches!(e, Event::ClientMessage(m) if m.type_ == wm_protocols)).count();
    assert_eq!(messages, 0, "a window listing no protocols got WM_PROTOCOLS messages");
}

/// X21: no `WM_HINTS` at all (Chromium, Electron, sxiv), or `WM_HINTS`
/// without the input flag (mupdf): it takes input.
#[test]
fn windows_without_an_input_hint_get_the_input_focus() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let bare = window_with(&x, None, &[]);
    until("a window without WM_HINTS never got the X focus", || x.input_focus() == bare);
    let window = x.window();
    WmHints::new().set(&x.conn, window).unwrap();
    x.map(window);
    until("a window without the input flag never got the X focus", || x.input_focus() == window);
}

/// X21: Locally Active (input and `WM_TAKE_FOCUS`, emacs-lucid): the X
/// focus and `WM_TAKE_FOCUS` both.
#[test]
fn locally_active_windows_get_the_focus_and_take_focus() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = window_with(&x, Some(true), &["WM_TAKE_FOCUS", "WM_DELETE_WINDOW"]);
    assert!(got_protocol(&x, window, "WM_TAKE_FOCUS", Duration::from_secs(2)), "no WM_TAKE_FOCUS");
    until("the window never got the X focus", || x.input_focus() == window);
}

/// X21: Globally Active (no input, `WM_TAKE_FOCUS`; IntelliJ's dialogs):
/// `WM_TAKE_FOCUS` only, and the app's own `SetInputFocus` stands.
#[test]
fn globally_active_windows_get_take_focus_and_keep_their_own_focus() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = window_with(&x, Some(false), &["WM_TAKE_FOCUS"]);
    assert!(got_protocol(&x, window, "WM_TAKE_FOCUS", Duration::from_secs(2)), "no WM_TAKE_FOCUS");
    x.conn.set_input_focus(InputFocus::PARENT, window, x11rb::CURRENT_TIME).unwrap();
    x.flush();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(x.input_focus(), window, "the window manager took the focus back");
}

/// X22: focus moving between two windows of one app goes straight from
/// one to the other, never to nothing in between (Unity's editor drops
/// to 10 fps on a focus loss).
#[test]
fn focus_moves_between_an_apps_windows_without_a_gap() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let a = x.mapped();
    let b = x.mapped();
    x.focus(a);
    x.conn.change_window_attributes(x.root, &ChangeWindowAttributesAux::new().event_mask(EventMask::FOCUS_CHANGE)).unwrap();
    x.flush();
    std::thread::sleep(Duration::from_millis(200));
    x.drain();
    x.activate(b, 1, a);
    until("the other window never got focus", || x.active() == Some(b));
    let gaps = x.events_for(Duration::from_millis(300)).iter().filter(|e| matches!(e, Event::FocusIn(f) if f.event == x.root && matches!(f.detail, NotifyDetail::NONE | NotifyDetail::POINTER_ROOT))).count();
    assert_eq!(gaps, 0, "the focus went to nothing between the app's windows");
}

/// X23: the app moves the X focus to a child of its focused window
/// (Unity): it is still the active window.
#[test]
fn focus_on_a_child_keeps_the_window_active() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.mapped();
    x.focus(window);
    let child = x.conn.generate_id().unwrap();
    x.conn.create_window(0, child, window, 10, 10, 50, 50, 0, WindowClass::INPUT_OUTPUT, 0, &CreateWindowAux::new()).unwrap();
    x.conn.map_window(child).unwrap();
    x.conn.set_input_focus(InputFocus::PARENT, child, x11rb::CURRENT_TIME).unwrap();
    x.flush();
    let deadline = Instant::now() + Duration::from_millis(700);
    while Instant::now() < deadline {
        assert_eq!(x.active(), Some(window), "_NET_ACTIVE_WINDOW left the window");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// X24: the X focus set to nothing or to the pointer's window: no X11
/// window is active (Wine locks the mouse on the active window).
#[test]
fn focus_on_nothing_clears_the_active_window() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.mapped();
    let other = x.mapped();
    x.focus(window);
    x.conn.set_input_focus(InputFocus::NONE, x11rb::NONE, x11rb::CURRENT_TIME).unwrap();
    x.flush();
    until("_NET_ACTIVE_WINDOW still names the window after focus went to none", || x.active() == Some(x11rb::NONE));
    x.focus(other);
    x.conn.set_input_focus(InputFocus::NONE, u32::from(InputFocus::POINTER_ROOT), x11rb::CURRENT_TIME).unwrap();
    x.flush();
    until("_NET_ACTIVE_WINDOW still names the window after focus went to the pointer root", || x.active() == Some(x11rb::NONE));
}

/// X25: with the focus following the pointer (PointerRoot), the window
/// under the pointer gets FocusIn (detail Pointer): that is not an
/// activation (VSCode's keys went to the wrong window).
#[test]
fn the_pointers_window_is_not_activated() {
    let Some(mut session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.mapped();
    x.focus(window);
    let (wx, wy) = x.position(window);
    session.input("move 790 590");
    x.conn.set_input_focus(InputFocus::NONE, u32::from(InputFocus::POINTER_ROOT), x11rb::CURRENT_TIME).unwrap();
    x.flush();
    until("_NET_ACTIVE_WINDOW still names the window", || x.active() == Some(x11rb::NONE));
    session.input(&format!("move {} {}", wx + 50, wy + 50));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(x.active(), Some(x11rb::NONE), "the window under the pointer became active");
}

/// X26: a menu grabs the keyboard (every X11 toolkit's menus): the app's
/// window stays the active one throughout, and keys go to the menu.
#[test]
fn a_menu_grabbing_the_keyboard_does_not_change_the_active_window() {
    let Some(mut session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.mapped();
    x.focus(window);
    let (wx, wy) = x.position(window);
    let menu = x.create_with(wx + 20, wy + 20, 100, 100, CreateWindowAux::new().override_redirect(1));
    x.conn.change_window_attributes(menu, &ChangeWindowAttributesAux::new().event_mask(EventMask::KEY_PRESS | EventMask::STRUCTURE_NOTIFY)).unwrap();
    x.conn.change_window_attributes(x.root, &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE)).unwrap();
    x.map(menu);
    std::thread::sleep(Duration::from_millis(300));
    x.drain();
    let grab = x.conn.grab_keyboard(false, menu, x11rb::CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC).unwrap().reply().unwrap();
    assert_eq!(grab.status, GrabStatus::SUCCESS, "the menu could not grab the keyboard");
    session.input("type ab");
    let active = x.atom("_NET_ACTIVE_WINDOW");
    let events = x.events_for(Duration::from_millis(700));
    let keys = events.iter().filter(|e| matches!(e, Event::KeyPress(k) if k.event == menu)).count();
    assert_eq!(keys, 2, "keys the grabbing menu got");
    x.conn.ungrab_keyboard(x11rb::CURRENT_TIME).unwrap();
    x.flush();
    let events: Vec<_> = events.into_iter().chain(x.events_for(Duration::from_millis(300))).collect();
    let changes = events.iter().filter(|e| matches!(e, Event::PropertyNotify(p) if p.window == x.root && p.atom == active)).count();
    assert_eq!(changes, 0, "_NET_ACTIVE_WINDOW changed during the menu's grab");
    assert_eq!(x.active(), Some(window));
}

/// X27: while a Wayland window has focus, an X11 menu can still grab the
/// keyboard (dmenu), so the X focus is not None.
#[test]
fn x11_grabs_work_while_a_wayland_window_has_focus() {
    let Some(session) = xwm::session() else { return };
    let mut app = common::connect(&session);
    let (wayland, ()) = app.toplevel((200, 100), |_, _| {});
    let surface = wayland.surface.clone();
    app.until("the Wayland window has no focus", |s| s.keyboard.as_ref() == Some(&surface));
    let x = X::new(&session);
    assert_ne!(x.input_focus(), x11rb::NONE, "the X focus is None");
    let menu = x.popup((300, 300), (100, 100));
    std::thread::sleep(Duration::from_millis(300));
    let grab = x.conn.grab_keyboard(false, menu, x11rb::CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC).unwrap().reply().unwrap();
    assert_eq!(grab.status, GrabStatus::SUCCESS, "an X11 menu could not grab the keyboard");
}

/// X28: `_NET_WM_STATE_FOCUSED` on the focused window only (Steam's
/// maximize button needs it).
#[test]
fn the_focused_window_is_marked_focused() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let a = x.mapped();
    let b = x.mapped();
    x.focus(a);
    until("the focused window is not marked focused", || x.has_state(a, "_NET_WM_STATE_FOCUSED"));
    x.focus(b);
    until("the focused window is not marked focused", || x.has_state(b, "_NET_WM_STATE_FOCUSED"));
    until("the window that lost focus is still marked focused", || !x.has_state(a, "_NET_WM_STATE_FOCUSED"));
}

/// X29: the focused window unmapped and destroyed at once (closing
/// Firefox): the focus moves on, and the window manager keeps working.
#[test]
fn the_focused_window_destroyed_at_once() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let other = x.mapped();
    let window = x.mapped();
    x.focus(window);
    x.conn.unmap_window(window).unwrap();
    x.conn.destroy_window(window).unwrap();
    x.flush();
    until("the focus did not move to the other window", || x.active() == Some(other));
    xwm::wm_still_works(&session);
}

/// X30: an app moves the X focus to another of its windows itself (Steam's
/// popups): it stays there.
#[test]
fn an_app_focusing_its_own_window_keeps_it() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let a = x.mapped();
    let b = x.mapped();
    x.focus(a);
    x.conn.set_input_focus(InputFocus::PARENT, b, x11rb::CURRENT_TIME).unwrap();
    x.flush();
    until("the app's own focus change did not show", || x.active() == Some(b));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(x.input_focus(), b, "the window manager moved the focus back");
    assert_eq!(x.active(), Some(b));
}

/// X31: an app moving the X focus around quickly: it settles; the window
/// manager and the app do not chase each other (Chrome with two windows
/// once kept Xwayland at 100% CPU).
#[test]
fn quick_focus_changes_settle() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let a = x.mapped();
    let b = x.mapped();
    x.focus(a);
    for _ in 0..5 {
        x.conn.set_input_focus(InputFocus::PARENT, b, x11rb::CURRENT_TIME).unwrap();
        x.conn.set_input_focus(InputFocus::PARENT, a, x11rb::CURRENT_TIME).unwrap();
    }
    x.flush();
    std::thread::sleep(Duration::from_millis(500));
    x.drain();
    let focus_events = x.events_for(Duration::from_secs(1)).iter().filter(|e| matches!(e, Event::FocusIn(_) | Event::FocusOut(_))).count();
    assert!(focus_events < 4, "{focus_events} focus events in a second after the app stopped");
}

/// X32: activating the active window again changes nothing: no
/// `_NET_WM_STATE` or stacking rewrites (every pager wakes up).
#[test]
fn activating_the_active_window_again_changes_nothing() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.mapped();
    x.focus(window);
    x.conn.change_window_attributes(x.root, &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE)).unwrap();
    x.flush();
    std::thread::sleep(Duration::from_millis(300));
    x.drain();
    for _ in 0..10 {
        x.activate(window, 2, x11rb::NONE);
    }
    let (state, stacking) = (x.atom("_NET_WM_STATE"), x.atom("_NET_CLIENT_LIST_STACKING"));
    let events = x.events_for(Duration::from_millis(700));
    let states = events.iter().filter(|e| matches!(e, Event::PropertyNotify(p) if p.window == window && p.atom == state)).count();
    let restacks = events.iter().filter(|e| matches!(e, Event::PropertyNotify(p) if p.window == x.root && p.atom == stacking)).count();
    let configures = events.iter().filter(|e| matches!(e, Event::ConfigureNotify(c) if c.window == window)).count();
    assert!(states <= 1, "{states} _NET_WM_STATE rewrites");
    assert_eq!((restacks, configures), (0, 0), "stacking rewrites, ConfigureNotify");
}

/// X33: pings only for windows that take part (`_NET_WM_PING` in
/// `WM_PROTOCOLS`); others would be taken for not responding.
#[test]
fn only_windows_listing_ping_are_pinged() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let plain = window_with(&x, Some(true), &["WM_DELETE_WINDOW"]);
    until("the window never got focus", || x.active() == Some(plain));
    assert!(!got_protocol(&x, plain, "_NET_WM_PING", Duration::from_secs(1)), "a window not listing _NET_WM_PING was pinged");
    let pinged = window_with(&x, Some(true), &["WM_DELETE_WINDOW", "_NET_WM_PING"]);
    until("the window never got focus", || x.active() == Some(pinged));
    assert!(got_protocol(&x, pinged, "_NET_WM_PING", Duration::from_secs(2)), "a window listing _NET_WM_PING was not pinged on focus");
}

/// X34: focus changes made by the compositor reach X11 apps at once,
/// without waiting for other X11 traffic.
#[test]
fn focus_changes_reach_x11_apps_at_once() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let window = x.mapped();
    x.focus(window);
    let mut app = common::connect(&session);
    let (wayland, ()) = app.toplevel((200, 100), |_, _| {});
    let surface = wayland.surface.clone();
    app.until("the Wayland window has no focus", |s| s.keyboard.as_ref() == Some(&surface));
    let deadline = Instant::now() + Duration::from_secs(1);
    while x.active() != Some(x11rb::NONE) {
        assert!(Instant::now() < deadline, "_NET_ACTIVE_WINDOW still names the X11 window a second after a Wayland window took focus");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// X94 (W30): an X11 window with an input shape (an OSD, a shaped
/// launcher): the pointer outside it goes through to the window below,
/// inside it reaches the X11 window.
#[test]
#[ignore = "fails today: X11 input shapes aren't honored; the pointer outside the shape still goes to the X11 window (#191)"]
fn the_pointer_outside_an_input_shape_goes_through() {
    use x11rb::protocol::shape::{ConnectionExt as _, SK, SO};
    use x11rb::protocol::xproto::{ClipOrdering, Rectangle};
    let Some(mut session) = xwm::session() else { return };
    // A big Wayland window below, and where it is.
    let mut app = common::connect(&session);
    let (wl, ()) = app.toplevel((700, 500), |_, _| {});
    let surface = wl.surface.clone();
    let mut over_wayland = Vec::new();
    for px in (0..i32::from(xwm::SCREEN.0)).step_by(30) {
        for py in (0..i32::from(xwm::SCREEN.1)).step_by(30) {
            session.input(&format!("move {px} {py}"));
            app.roundtrip();
            if app.state.pointer.as_ref() == Some(&surface) {
                over_wayland.push((px, py));
            }
        }
    }
    let x = X::new(&session);
    let window = x.window();
    let events = EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE | EventMask::ENTER_WINDOW | EventMask::LEAVE_WINDOW | EventMask::POINTER_MOTION;
    x.conn.change_window_attributes(window, &ChangeWindowAttributesAux::new().event_mask(events)).unwrap();
    x.map_managed(window);
    let (wx, wy) = x.position(window);
    let (ww, wh) = x.size(window);
    let (wx, wy, ww, wh) = (i32::from(wx), i32::from(wy), i32::from(ww), i32::from(wh));
    // Input only in its left half.
    let left = Rectangle { x: 0, y: 0, width: (ww / 2) as u16, height: wh as u16 };
    x.conn.shape_rectangles(SO::SET, SK::INPUT, ClipOrdering::UNSORTED, window, 0, 0, &[left]).unwrap();
    x.flush();
    std::thread::sleep(Duration::from_millis(300));
    let in_right = |(px, py): &(i32, i32)| *px >= wx + ww / 2 + 10 && *px < wx + ww - 10 && *py >= wy + 10 && *py < wy + wh - 10;
    let in_left = |(px, py): &(i32, i32)| *px >= wx + 10 && *px < wx + ww / 2 - 10 && *py >= wy + 10 && *py < wy + wh - 10;
    let right = *over_wayland.iter().find(|p| in_right(p)).expect("the Wayland window isn't under the X11 window's right half");
    let left = *over_wayland.iter().find(|p| in_left(p)).expect("the Wayland window isn't under the X11 window's left half");

    session.input(&format!("move {} {}", left.0, left.1));
    app.roundtrip();
    x.wait_event("the X11 window never got the pointer inside its shape", |e| matches!(e, Event::EnterNotify(n) if n.event == window).then_some(()));
    session.input(&format!("move {} {}", right.0, right.1));
    app.until("the pointer outside the input shape never reached the Wayland window below", |s| s.pointer.as_ref() == Some(&surface));
}
