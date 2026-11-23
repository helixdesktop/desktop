//! X11 windows wanting attention (helixdesktop/spec
//! `contracts/wayland/README.md`, "Windows wanting attention"): the
//! `WM_HINTS` urgency hint (xterm's `bellIsUrgent`, IRC and chat clients,
//! Thunderbird) and `_NET_WM_STATE_DEMANDS_ATTENTION` (Firefox, Qt,
//! Java) mark a window in the shell until it gets focus or asks no more.
//! Under either window manager (`HELIX_XWM`); the shell is the test.

mod common;

use common::xwm::{self, until, Shell, X};
use x11rb::properties::WmHints;
use x11rb::protocol::xproto::Window;

/// Two windows: `background`, then `front` (focused) over it.
fn two_windows(x: &X, shell: &mut Shell) -> (Window, u32, Window) {
    let background = x.mapped_titled("background");
    let front = x.mapped_titled("front");
    x.focus(front);
    let id = shell.window("background").id;
    shell.window("front");
    (background, id, front)
}

fn set_urgent(x: &X, window: Window, urgent: bool) {
    let mut hints = WmHints::new();
    hints.input = Some(true);
    hints.urgent = urgent;
    hints.set(&x.conn, window).unwrap();
    x.flush();
}

/// The urgency hint on a window in the background marks it; taking the
/// hint away unmarks it.
#[test]
fn the_urgency_hint_wants_attention() {
    let Some((session, mut shell)) = xwm::session_with_shell("x11-urgent") else { return };
    let x = X::new(&session);
    let (background, id, _) = two_windows(&x, &mut shell);
    assert!(!shell.state.attention.contains(&id), "a window wants attention before asking");
    set_urgent(&x, background, true);
    shell.until("the urgency hint did not mark the window", |s| s.attention.contains(&id));
    set_urgent(&x, background, false);
    shell.until("taking the urgency hint away did not unmark the window", |s| !s.attention.contains(&id));
    // And again: focus unmarks it too.
    set_urgent(&x, background, true);
    shell.until("the urgency hint set again did not mark the window", |s| s.attention.contains(&id));
    x.focus(background);
    shell.until("focus did not unmark the window", |s| !s.attention.contains(&id));
}

/// `_NET_WM_STATE_DEMANDS_ATTENTION` asked for (as a client message, as
/// EWMH has it) marks a window in the background; focus unmarks it.
#[test]
fn demands_attention_wants_attention() {
    let Some((session, mut shell)) = xwm::session_with_shell("x11-demands") else { return };
    let x = X::new(&session);
    let (background, id, _) = two_windows(&x, &mut shell);
    x.net_wm_state(background, 1, &["_NET_WM_STATE_DEMANDS_ATTENTION"]);
    shell.until("_NET_WM_STATE_DEMANDS_ATTENTION did not mark the window", |s| s.attention.contains(&id));
    x.net_wm_state(background, 0, &["_NET_WM_STATE_DEMANDS_ATTENTION"]);
    shell.until("removing _NET_WM_STATE_DEMANDS_ATTENTION did not unmark the window", |s| !s.attention.contains(&id));
    x.net_wm_state(background, 2, &["_NET_WM_STATE_DEMANDS_ATTENTION"]);
    shell.until("toggling _NET_WM_STATE_DEMANDS_ATTENTION on did not mark the window", |s| s.attention.contains(&id));
    x.focus(background);
    shell.until("focus did not unmark the window", |s| !s.attention.contains(&id));
}

/// The focused window asking for attention isn't marked: it has it.
/// Each time the window in the background asks right after, through the
/// same path, so once that one is marked the focused one's request has
/// been handled.
#[test]
fn the_focused_window_is_not_marked() {
    let Some((session, mut shell)) = xwm::session_with_shell("x11-focused-urgent") else { return };
    let x = X::new(&session);
    let (background, background_id, front) = two_windows(&x, &mut shell);
    let id = shell.window("front").id;
    set_urgent(&x, front, true);
    set_urgent(&x, background, true);
    shell.until("the urgency hint did not mark the window in the background", |s| s.attention.contains(&background_id));
    assert!(!shell.state.attention.contains(&id), "the focused window was marked for its urgency hint");
    set_urgent(&x, background, false);
    shell.until("taking the urgency hint away did not unmark the window", |s| !s.attention.contains(&background_id));
    x.net_wm_state(front, 1, &["_NET_WM_STATE_DEMANDS_ATTENTION"]);
    x.net_wm_state(background, 1, &["_NET_WM_STATE_DEMANDS_ATTENTION"]);
    shell.until("_NET_WM_STATE_DEMANDS_ATTENTION did not mark the window in the background", |s| s.attention.contains(&background_id));
    assert!(!shell.state.attention.contains(&id), "the focused window was marked for _NET_WM_STATE_DEMANDS_ATTENTION");
    until("the window lost focus", || x.active() == Some(front));
}
