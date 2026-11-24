//! X11 startup notification ids (`tests/x11_cases.md` X89, X138;
//! helixdesktop/spec `contracts/wayland/README.md`, "Windows wanting
//! attention"): an app the shell launches or wakes is handed an activation
//! token as its startup notification id (`DESKTOP_STARTUP_ID`, or
//! `desktop-startup-id` for one already running). An X11 app asking for its
//! window to be activated with that token as the window's `_NET_STARTUP_ID`
//! (or its client leader's, where GTK 3 puts it) gets focus, as the token
//! would on Wayland; once. Other requests only make the window want
//! attention. Under either window manager (`HELIX_XWM`); the shell is the
//! test.

mod common;

use common::xwm::{self, until, Shell, X};
use x11rb::protocol::xproto::Window;

/// Two windows: `background`, then `front` (focused) over it.
fn two_windows(x: &X, shell: &mut Shell) -> (Window, u32, Window) {
    let background = x.mapped_titled("background");
    let front = x.mapped_titled("front");
    x.focus(front);
    let id = shell.window("background").id;
    shell.window("front");
    shell.until("the front window has no focus", |s| s.focused.is_some_and(|f| f != id));
    (background, id, front)
}

fn set_startup_id(x: &X, window: Window, id: &str) {
    x.set8(window, "_NET_STARTUP_ID", x.atom("UTF8_STRING"), id.as_bytes());
}

/// `window` asks to be activated, as an app presenting it does (source 1,
/// no active window of its own).
fn ask_activation(x: &X, window: Window) {
    x.activate(window, 1, x11rb::NONE);
    x.flush();
}

/// The request is not granted: the window wants attention, and focus
/// stays where it was.
fn wants_attention(x: &X, shell: &mut Shell, window: Window, id: u32, what: &str) {
    shell.until(&format!("{what}: the window does not want attention"), |s| s.attention.contains(&id));
    assert_ne!(shell.state.focused, Some(id), "{what}: the window took focus");
    assert_ne!(x.active(), Some(window), "{what}: the window is the active X11 window");
}

fn activated(x: &X, shell: &mut Shell, window: Window, id: u32, what: &str) {
    shell.until(&format!("{what}: the window did not get focus"), |s| s.focused == Some(id));
    until(&format!("{what}: _NET_ACTIVE_WINDOW does not name the window"), || x.active() == Some(window));
    assert!(!shell.state.attention.contains(&id), "{what}: the focused window wants attention");
}

/// The shell's token on the window itself (Qt, GTK 3's
/// `gtk_window_set_startup_id`): activated; the token is used up, so the
/// same request later only wants attention.
#[test]
fn the_shells_token_activates_the_window_once() {
    let Some((session, mut shell)) = xwm::session_with_shell("x11-startup-id") else { return };
    let x = X::new(&session);
    let (background, id, front) = two_windows(&x, &mut shell);
    let token = shell.token();
    set_startup_id(&x, background, &token);
    ask_activation(&x, background);
    activated(&x, &mut shell, background, id, "the shell's token as _NET_STARTUP_ID");

    // Back to the front window; the same token again.
    x.focus(front);
    shell.until("the front window did not get focus back", |s| s.focused.is_some_and(|f| f != id));
    ask_activation(&x, background);
    wants_attention(&x, &mut shell, background, id, "a token used before");
}

/// The shell's token on the window's client leader (GTK 3 sets it there
/// when a running app is asked to present a window for a new launch):
/// activated.
#[test]
fn the_shells_token_on_the_client_leader_activates_the_window() {
    let Some((session, mut shell)) = xwm::session_with_shell("x11-startup-leader") else { return };
    let x = X::new(&session);
    let leader = x.window();
    let background = x.window();
    x.set8(background, "_NET_WM_NAME", x.atom("UTF8_STRING"), b"background");
    x.set32(background, "WM_CLIENT_LEADER", x11rb::protocol::xproto::AtomEnum::WINDOW, &[leader]);
    x.map_managed(background);
    let front = x.mapped_titled("front");
    x.focus(front);
    let id = shell.window("background").id;
    shell.until("the front window has no focus", |s| s.focused.is_some_and(|f| f != id));

    let token = shell.token();
    set_startup_id(&x, leader, &token);
    ask_activation(&x, background);
    activated(&x, &mut shell, background, id, "the shell's token on the client leader");
}

/// No startup id, or one that is not a token Helix made: the window only
/// wants attention.
#[test]
fn other_startup_ids_only_want_attention() {
    let Some((session, mut shell)) = xwm::session_with_shell("x11-startup-other") else { return };
    let x = X::new(&session);
    let (background, id, _) = two_windows(&x, &mut shell);
    ask_activation(&x, background);
    wants_attention(&x, &mut shell, background, id, "no startup id");

    let (background, id, _) = {
        let b = x.mapped_titled("second");
        let f = x.mapped_titled("third");
        x.focus(f);
        let id = shell.window("second").id;
        shell.until("the third window has no focus", |s| s.focused.is_some_and(|f| f != id));
        (b, id, f)
    };
    // A startup id of the X11 kind (as an X11 launcher makes), not a token.
    set_startup_id(&x, background, "xterm-1234-host-xterm-0_TIME5678");
    ask_activation(&x, background);
    wants_attention(&x, &mut shell, background, id, "a startup id that is not a token");
}
