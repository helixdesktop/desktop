//! A window Move or Resize the session lock ends halfway (#239): the
//! lock screen gets the keyboard (keyboard Move/Resize from the window
//! menu used to keep driving it, so the password field got no keys), and
//! an interrupted resize leaves no Resizing state behind nor snaps the
//! window back where the resize began later.
//!
//! Runs the real `helix-comp` headless with the test as its shell (the
//! window list, and the session lock); needs `dbus-daemon` and Xwayland
//! (the stand-in shell's session), else skipped.

mod common;

use common::xwm;
use wayland_protocols::xdg::shell::client::xdg_toplevel;

/// helix_window_list_v1 actions.
const BEGIN_MOVE: u32 = 9;
const BEGIN_RESIZE: u32 = 10;

/// Keys (evdev codes), as the headless input sends them.
const KEY_A: u32 = 30;
const KEY_RIGHT: u32 = 106;
const KEY_ENTER: u32 = 28;

fn follow_then_lock(action: u32, name: &str) {
    let Some((mut session, mut shell)) = xwm::session_with_shell(name) else { return };
    let mut app = common::connect(&session);
    let (_window, ()) = app.toplevel((300, 200), |_, t| t.toplevel.set_title("follow".into()));
    let id = shell.window("follow").id;
    shell.action(id, action);
    let lock = shell.lock();
    for _ in 0..3 {
        session.input(&format!("key {KEY_A} 1"));
        session.input(&format!("key {KEY_A} 0"));
    }
    shell.until("the lock screen never got the keys typed on it", |s| s.lock_keys.iter().filter(|k| **k == KEY_A).count() == 3);
    shell.unlock(lock);
}

#[test]
fn the_lock_screen_gets_the_keyboard_during_a_window_menu_move() {
    follow_then_lock(BEGIN_MOVE, "follow-lock-move");
}

#[test]
fn the_lock_screen_gets_the_keyboard_during_a_window_menu_resize() {
    follow_then_lock(BEGIN_RESIZE, "follow-lock-resize");
}

#[test]
fn a_resize_the_lock_interrupts_leaves_nothing_behind() {
    let Some((mut session, mut shell)) = xwm::session_with_shell("resize-lock") else { return };
    let mut app = common::connect(&session);
    let size = (300, 200);
    let (window, ()) = app.toplevel(size, |_, t| t.toplevel.set_title("resized".into()));
    let id = shell.window("resized").id;
    // Grab the window and start resizing it from its top-left corner.
    let (x, y) = common::point_over(&mut session, &mut app, &window.surface, (xwm::SCREEN.0.into(), xwm::SCREEN.1.into()));
    session.input("button 272 1");
    app.until("the press never reached the window", |s| s.press.is_some());
    let serial = app.state.press.unwrap();
    window.toplevel.resize(&app.seat, serial, xdg_toplevel::ResizeEdge::TopLeft);
    app.roundtrip();
    session.input(&format!("move {} {}", x - 20, y - 20));
    app.until("the window was never told it is being resized", |s| s.resizing);

    // The session locks halfway: the resize is over.
    let lock = shell.lock();
    app.until("the window is still told it is being resized after the lock ended the resize", |s| !s.resizing);
    session.input("button 272 0");
    shell.unlock(lock);

    // Moved somewhere else, the window stays there when it draws again
    // (an unfinished resize put it back where the resize began).
    app.redraw(&window, size);
    let before = shell.window("resized").geometry;
    // Window-menu Move, three steps right with the keyboard, Return.
    shell.action(id, BEGIN_MOVE);
    for _ in 0..3 {
        session.input(&format!("key {KEY_RIGHT} 1"));
        session.input(&format!("key {KEY_RIGHT} 0"));
    }
    session.input(&format!("key {KEY_ENTER} 1"));
    session.input(&format!("key {KEY_ENTER} 0"));
    shell.until("the window never moved", |s| s.windows.iter().any(|w| w.title == "resized" && w.geometry != before));
    let moved = shell.window("resized").geometry;
    for _ in 0..3 {
        app.redraw(&window, size);
        shell.roundtrip();
        assert_eq!(shell.window("resized").geometry, moved, "the window snapped back after it drew");
    }
}
