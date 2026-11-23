//! Where a window is: shown, minimized or on another workspace, and only
//! one of them (#240). With X11 windows, under either window manager
//! (`HELIX_XWM`); the shell is the test (the window list), whose actions
//! reach the compositor in order on one connection.

mod common;

use common::xwm::{action, session_with_shell, session_with_shell_env, until, X, SCREEN};
use x11rb::protocol::xproto::ConnectionExt as _;

/// An app minimizing its minimized window changes nothing: shown again,
/// it is listed once (a second minimize used to add a second entry at
/// (0, 0), which came back on the next focus). A Wayland app here: the X11
/// window managers already ignore a second Iconic request.
#[test]
fn a_window_minimized_twice_comes_back_once() {
    let Some((session, mut shell)) = session_with_shell("presence-twice") else { return };
    let mut app = common::connect(&session);
    let (window, ()) = app.toplevel((200, 150), |_, t| t.toplevel.set_title("twice".into()));
    let id = shell.window("twice").id;
    window.toplevel.set_minimized();
    app.roundtrip();
    shell.until("the window was not minimized", |s| s.windows.iter().any(|w| w.title == "twice" && w.minimized));
    window.toplevel.set_minimized();
    app.roundtrip();
    shell.activate(id);
    shell.until("the window is not listed once, shown", |s| {
        let listed: Vec<_> = s.windows.iter().filter(|w| w.title == "twice").collect();
        listed.len() == 1 && !listed[0].minimized
    });
    // Focus goes elsewhere and back: the window stays where it is.
    let shown = shell.window("twice").geometry;
    let _other = app.toplevel((100, 100), |_, t| t.toplevel.set_title("other".into()));
    let other = shell.window("other").id;
    shell.activate(other);
    shell.activate(id);
    shell.roundtrip();
    assert_eq!(shell.window("twice").geometry, shown, "the window moved when focused again");
    assert_eq!(shell.listed("twice"), 1, "the window is listed twice");
}

/// A minimized window asked to maximize stays minimized, and is maximized
/// when shown again (it used to show up while still listed as minimized).
#[test]
fn a_minimized_window_maximized_stays_hidden() {
    let Some((session, mut shell)) = session_with_shell("presence-maximize") else { return };
    let x = X::new(&session);
    let window = x.mapped_titled("hidden");
    let id = shell.window("hidden").id;
    shell.action(id, action::MINIMIZE);
    shell.action(id, action::MAXIMIZE);
    until("the window was not told it is maximized", || x.has_state(window, "_NET_WM_STATE_MAXIMIZED_VERT"));
    shell.until("the window is not listed once, minimized", |s| {
        let listed: Vec<_> = s.windows.iter().filter(|w| w.title == "hidden").collect();
        listed.len() == 1 && listed[0].minimized
    });
    assert_eq!(x.wm_state(window), Some(3), "the window is not Iconic any more");
    shell.activate(id);
    until("the window was not shown again", || x.wm_state(window) == Some(1));
    until("the window was not shown maximized", || x.size(window).0 == SCREEN.0);
    shell.until("the window is not listed once, shown", |s| {
        let listed: Vec<_> = s.windows.iter().filter(|w| w.title == "hidden").collect();
        listed.len() == 1 && !listed[0].minimized
    });
}

/// Show Desktop again brings back the windows it hid, even when one of
/// them was closed meanwhile (it used to minimize everything again).
#[test]
fn show_desktop_brings_back_the_windows_left() {
    let Some((session, mut shell)) = session_with_shell("presence-desktop") else { return };
    let x = X::new(&session);
    let kept = x.mapped_titled("kept");
    let closed = x.mapped_titled("closed");
    let id = shell.window("kept").id;
    shell.action(id, action::SHOW_DESKTOP);
    until("Show Desktop did not minimize the windows", || x.wm_state(kept) == Some(3) && x.wm_state(closed) == Some(3));
    x.conn.destroy_window(closed).unwrap();
    x.flush();
    shell.until("the closed window is still listed", |s| !s.windows.iter().any(|w| w.title == "closed"));
    shell.action(id, action::SHOW_DESKTOP);
    until("Show Desktop did not bring the window back", || x.wm_state(kept) == Some(1));
    shell.until("the window is not listed once, shown", |s| {
        let listed: Vec<_> = s.windows.iter().filter(|w| w.title == "kept").collect();
        listed.len() == 1 && !listed[0].minimized
    });
}

/// A window minimized on a monitor that is then unplugged comes back on
/// one that is still there (it used to come back off screen).
#[test]
fn a_minimized_window_comes_back_on_a_monitor_left() {
    let env = vec![("HELIX_HEADLESS_OUTPUTS".to_owned(), "2".to_owned())];
    let Some((mut session, mut shell)) = session_with_shell_env("presence-unplug", env) else { return };
    let x = X::new(&session);
    // New windows open on the monitor under the pointer: the second one.
    session.input(&format!("move {} {}", SCREEN.0 + SCREEN.0 / 2, SCREEN.1 / 2));
    let window = x.mapped_titled("away");
    until("the window did not open on the second monitor", || x.position(window).0 >= SCREEN.0 as i16);
    let id = shell.window("away").id;
    shell.action(id, action::MINIMIZE);
    until("the window was not minimized", || x.wm_state(window) == Some(3));
    session.input("unplug HEADLESS-2");
    shell.activate(id);
    until("the window was not shown again", || x.wm_state(window) == Some(1));
    until("the window came back on the monitor that went away", || {
        let (x0, y0) = x.position(window);
        x0 >= 0 && (x0 as u16) < SCREEN.0 && y0 >= 0 && (y0 as u16) < SCREEN.1
    });
}

/// The same session still works with one monitor (sanity for the test
/// command: unplugging the second monitor keeps the first).
#[test]
fn unplugging_leaves_the_first_monitor() {
    let env = vec![("HELIX_HEADLESS_OUTPUTS".to_owned(), "2".to_owned())];
    let Some((mut session, _shell)) = session_with_shell_env("presence-unplug-sanity", env) else { return };
    session.input("unplug HEADLESS-2");
    let x = X::new(&session);
    let window = x.mapped();
    let (x0, _) = x.position(window);
    assert!(x0 >= 0 && (x0 as u16) < SCREEN.0, "a new window opened off the monitor left: {x0}");
}
