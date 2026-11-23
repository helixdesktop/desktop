//! X11 apps that stop answering `_NET_WM_PING` (helixdesktop/spec
//! `contracts/wayland/README.md`, "Unresponsive apps"): an app is pinged
//! when its window gets focus; one that answered before and now doesn't
//! within `check-alive-timeout` is flagged to the shell, which asks
//! whether to wait or force it to quit. An app that never answered isn't
//! flagged (Zoom lists the protocol and never answers: `alive.rs`). Under
//! either window manager (`HELIX_XWM`); the shell is the test.

mod common;

use common::xwm::{self, action, until, Shell, X};
use x11rb::protocol::xproto::{ClientMessageEvent, ConnectionExt, EventMask, Window};
use x11rb::protocol::Event;

/// A short timeout, so a hung app is flagged soon.
const SETTINGS: &str = "[org/gnome/mutter]\ncheck-alive-timeout=uint32 1000\n";

/// The next `_NET_WM_PING` `window` gets: its message data.
fn next_ping(x: &X, window: Window) -> [u32; 5] {
    let (wm_protocols, ping) = (x.atom("WM_PROTOCOLS"), x.atom("_NET_WM_PING"));
    x.wait_event("the window was never pinged", |e| match e {
        Event::ClientMessage(m) if m.window == window && m.type_ == wm_protocols && m.data.as_data32()[0] == ping => Some(m.data.as_data32()),
        _ => None,
    })
}

/// Answer a ping as EWMH has it: the message sent back to the root.
fn pong(x: &X, data: [u32; 5]) {
    let message = ClientMessageEvent::new(32, x.root, x.atom("WM_PROTOCOLS"), data);
    x.conn.send_event(false, x.root, EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT, message).unwrap();
    x.flush();
}

/// An app whose window `hung` answers the ping it gets when shown, then
/// (after another window took focus and it got it back) stops answering:
/// flagged not responding. Returns its connection, window, the window's
/// id in the window list and its unanswered ping.
fn hung_app(session: &helix_test_support::headless::HeadlessSession, shell: &mut Shell) -> (X, Window, u32, [u32; 5]) {
    let app = X::new(session);
    let window = app.window();
    app.set_atoms(window, "WM_PROTOCOLS", &["WM_DELETE_WINDOW", "_NET_WM_PING"]);
    app.set8(window, "_NET_WM_NAME", app.atom("UTF8_STRING"), b"hung");
    app.map(window);
    pong(&app, next_ping(&app, window));
    let id = shell.window("hung").id;
    let other = X::new(session);
    let other_window = other.mapped_titled("other");
    other.focus(other_window);
    app.focus(window);
    let unanswered = next_ping(&app, window);
    shell.until("an app that stopped answering pings was not flagged", |s| s.not_responding.contains(&id));
    (app, window, id, unanswered)
}

/// Not answering: flagged; Force Quit disconnects the app, and its
/// window goes.
#[test]
fn a_hung_x11_app_is_flagged_and_can_be_forced_to_quit() {
    let Some((session, mut shell)) = xwm::session_with_shell_settings("x11-hung", Vec::new(), SETTINGS) else { return };
    let (app, _, id, _) = hung_app(&session, &mut shell);
    shell.action(id, action::FORCE_QUIT);
    until("Force Quit did not disconnect the X11 app", || app.conn.get_input_focus().ok().and_then(|c| c.reply().ok()).is_none());
    shell.until("the window of an app forced to quit is still listed", |s| !s.windows.iter().any(|w| w.id == id));
}

/// Answering late, after it was flagged: no longer flagged.
#[test]
fn a_hung_x11_app_answering_late_is_responding_again() {
    let Some((session, mut shell)) = xwm::session_with_shell_settings("x11-late", Vec::new(), SETTINGS) else { return };
    let (app, _, id, unanswered) = hung_app(&session, &mut shell);
    pong(&app, unanswered);
    shell.until("an app answering late is still flagged", |s| !s.not_responding.contains(&id));
    assert!(app.answers(), "the app was disconnected");
}

/// An app that never answers (the first ping it ever got went
/// unanswered) isn't flagged.
#[test]
fn an_x11_app_that_never_answers_is_not_flagged() {
    let Some((session, mut shell)) = xwm::session_with_shell_settings("x11-silent", Vec::new(), SETTINGS) else { return };
    let app = X::new(&session);
    let window = app.window();
    app.set_atoms(window, "WM_PROTOCOLS", &["WM_DELETE_WINDOW", "_NET_WM_PING"]);
    app.set8(window, "_NET_WM_NAME", app.atom("UTF8_STRING"), b"silent");
    app.map(window);
    next_ping(&app, window);
    let id = shell.window("silent").id;
    // Past the timeout, and a window that answers flagged meanwhile shows
    // the check ran: no flag for this one.
    let (_other, _, other_id, _) = hung_app(&session, &mut shell);
    assert!(shell.state.not_responding.contains(&other_id));
    assert!(!shell.state.not_responding.contains(&id), "an app that never answered a ping was flagged");
}
