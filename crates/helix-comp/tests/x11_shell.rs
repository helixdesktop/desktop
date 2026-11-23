//! X11 windows as the shell sees them (Helix's window list), and the
//! shell acting on them: the edge cases of `tests/x11_cases.md` about
//! titles, classes, properties set before mapping, and closing (#191).
//!
//! Runs the real `helix-comp` headless; needs `dbus-daemon` and Xwayland
//! (as every headless session), skipped otherwise.

mod common;

use std::time::Duration;

use common::xwm::{self, action, until, X};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};
use x11rb::protocol::Event;

/// X54: the title is `_NET_WM_NAME` while there is one, `WM_NAME` once it
/// is gone (not empty); `WM_NAME` is Latin-1 (Firefox's "Actualités").
#[test]
fn titles_fall_back_and_decode() {
    let Some((session, mut shell)) = xwm::session_with_shell("x11-titles") else { return };
    let x = X::new(&session);
    let window = x.window();
    x.set8(window, "_NET_WM_NAME", x.atom("UTF8_STRING"), "Préférences".as_bytes());
    x.set8(window, "WM_NAME", AtomEnum::STRING, b"fallback");
    x.map_managed(window);
    let id = shell.window("Préférences").id;
    x.delete(window, "_NET_WM_NAME");
    shell.until("the title did not fall back to WM_NAME", |s| s.windows.iter().any(|w| w.id == id && w.title == "fallback"));
    x.set8(window, "WM_NAME", AtomEnum::STRING, b"Actualit\xe9s");
    shell.until("WM_NAME is not read as Latin-1", |s| s.windows.iter().any(|w| w.id == id && w.title == "Actualités"));
}

/// X55: `WM_CLASS` is instance then class; the class names the app, also
/// when the property is short or unterminated.
#[test]
fn the_class_names_the_app() {
    let Some((session, mut shell)) = xwm::session_with_shell("x11-class") else { return };
    let x = X::new(&session);
    let window = x.window();
    x.set8(window, "WM_CLASS", AtomEnum::STRING, b"gimp\0Gimp\0");
    x.set8(window, "_NET_WM_NAME", x.atom("UTF8_STRING"), b"classed");
    x.map_managed(window);
    let listed = shell.window("classed");
    assert_eq!(listed.app_id, "Gimp", "the app id is not WM_CLASS's class");
    for value in [&b"solo"[..], b"solo\0", b"inst\0Klass", b""] {
        x.set8(window, "WM_CLASS", AtomEnum::STRING, value);
    }
    std::thread::sleep(Duration::from_millis(300));
    xwm::wm_still_works(&session);
}

/// X56: everything set before mapping is there when the window first
/// shows: title, class, and the shell is told once.
#[test]
fn properties_set_before_mapping_show_at_once() {
    let Some((session, mut shell)) = xwm::session_with_shell("x11-premap") else { return };
    let x = X::new(&session);
    let window = x.window();
    x.set8(window, "WM_CLASS", AtomEnum::STRING, b"premap\0Premap\0");
    x.set8(window, "_NET_WM_NAME", x.atom("UTF8_STRING"), b"set before mapping");
    x.map_managed(window);
    let listed = shell.window("set before mapping");
    assert_eq!(listed.app_id, "Premap");
}

/// X57: closing a window that takes part asks it (`WM_DELETE_WINDOW`).
#[test]
fn closing_asks_a_window_that_takes_part() {
    let Some((session, mut shell)) = xwm::session_with_shell("x11-close") else { return };
    let polite = X::new(&session);
    let window = polite.window();
    polite.set_atoms(window, "WM_PROTOCOLS", &["WM_DELETE_WINDOW"]);
    polite.set8(window, "_NET_WM_NAME", polite.atom("UTF8_STRING"), b"polite");
    polite.map_managed(window);
    let id = shell.window("polite").id;
    polite.drain();
    shell.action(id, action::CLOSE);
    let (wm_protocols, delete) = (polite.atom("WM_PROTOCOLS"), polite.atom("WM_DELETE_WINDOW"));
    polite.wait_event("closing never asked the window", |e| {
        matches!(e, Event::ClientMessage(m) if m.window == window && m.type_ == wm_protocols && m.data.as_data32()[0] == delete).then_some(())
    });
}

/// X57: closing a window whose app does not take part, or no longer does
/// (`WM_PROTOCOLS` deleted), disconnects the app (ICCCM: `KillClient`),
/// rather than leaving it running without its window.
#[test]
fn closing_disconnects_an_app_that_does_not_take_part() {
    let Some((session, mut shell)) = xwm::session_with_shell("x11-kill") else { return };
    let rude = X::new(&session);
    let window = rude.window();
    rude.set8(window, "_NET_WM_NAME", rude.atom("UTF8_STRING"), b"rude");
    rude.map_managed(window);
    let id = shell.window("rude").id;
    shell.action(id, action::CLOSE);
    until("closing a window without WM_DELETE_WINDOW did not disconnect its app", || rude.conn.get_input_focus().ok().and_then(|c| c.reply().ok()).is_none());

    // Took part, then stopped (WM_PROTOCOLS deleted).
    let former = X::new(&session);
    let window = former.window();
    former.set_atoms(window, "WM_PROTOCOLS", &["WM_DELETE_WINDOW"]);
    former.set8(window, "_NET_WM_NAME", former.atom("UTF8_STRING"), b"former");
    former.map_managed(window);
    let id = shell.window("former").id;
    former.delete(window, "WM_PROTOCOLS");
    std::thread::sleep(Duration::from_millis(300));
    shell.action(id, action::CLOSE);
    until("closing a window that stopped listing WM_DELETE_WINDOW did not disconnect its app", || former.conn.get_input_focus().ok().and_then(|c| c.reply().ok()).is_none());
    let _ = former.conn.flush();
}
