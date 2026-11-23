//! The X stacking order, as X11 apps see it: the edge cases of
//! `tests/x11_cases.md` about the X stack and `_NET_CLIENT_LIST_STACKING`
//! following what is shown on screen (Chromium and Electron find drop
//! targets by it), and override-redirect windows staying on top (#191).
//!
//! Runs the real `helix-comp` headless; needs `dbus-daemon` and Xwayland
//! (as every headless session), skipped otherwise.

mod common;

use common::xwm::{self, action, until, X};
use x11rb::protocol::xproto::Window;

/// The X stack and `_NET_CLIENT_LIST_STACKING` agree about `windows`.
fn stacks_agree(x: &X, windows: &[Window]) -> bool {
    let listed: Vec<Window> = x.stacking_list().into_iter().filter(|w| windows.contains(w)).collect();
    listed == x.stacking_order(windows)
}

/// X58: the focused (raised) window is on top of the X stack too (drops
/// from Slack went through to Discord underneath).
#[test]
fn the_x_stack_follows_focus() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let windows = [x.mapped(), x.mapped(), x.mapped()];
    for &w in windows.iter().chain([&windows[0]]) {
        x.focus(w);
        until("the focused window is not on top of the X stack", || x.stacking_order(&windows).last() == Some(&w));
    }
}

/// X58: `_NET_CLIENT_LIST_STACKING` is the X stack, each window once
/// (Chromium and Electron read it to find drop targets).
#[test]
fn the_stacking_list_is_the_x_stack() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let windows = [x.mapped(), x.mapped(), x.mapped()];
    for &w in windows.iter().chain([&windows[0]]) {
        x.focus(w);
        until("the focused window is not on top of the X stack", || x.stacking_order(&windows).last() == Some(&w));
        until("_NET_CLIENT_LIST_STACKING is not the X stack", || stacks_agree(&x, &windows));
    }
}

/// X58: raising and lowering from the shell shows in the X stack.
#[test]
fn the_x_stack_follows_raise_and_lower() {
    let Some((session, mut shell)) = xwm::session_with_shell("x11-stack") else { return };
    let x = X::new(&session);
    let a = x.mapped_titled("a");
    let b = x.mapped_titled("b");
    let (ia, ib) = (shell.window("a").id, shell.window("b").id);
    shell.action(ib, action::LOWER);
    until("lowering did not show in the X stack", || x.stacking_order(&[a, b]) == vec![b, a]);
    shell.action(ib, action::RAISE);
    until("raising did not show in the X stack", || x.stacking_order(&[a, b]) == vec![a, b]);
    shell.action(ia, action::RAISE);
    until("raising did not show in the X stack", || x.stacking_order(&[a, b]) == vec![b, a]);
}

/// X59: raising an app window keeps it under the menus and tooltips
/// (override-redirect windows) already shown.
#[test]
fn raised_windows_stay_under_menus() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let a = x.mapped();
    let b = x.mapped();
    let (ax, ay) = x.position(a);
    let menu = x.popup((ax + 20, ay + 20), (100, 100));
    x.sync();
    x.focus(a);
    x.focus(b);
    x.focus(a);
    x.sync();
    let order = x.stacking_order(&[a, b, menu]);
    assert_eq!(order.last(), Some(&menu), "a raised window went over the menu: {order:?}");
}

/// X60: a window without a parent that an app shows over its own
/// fullscreen window (Chromium's context menus) is above it.
#[test]
fn a_window_over_a_fullscreen_window_is_above_it() {
    let Some(session) = xwm::session() else { return };
    let x = X::new(&session);
    let full = x.mapped();
    x.net_wm_state(full, 1, &["_NET_WM_STATE_FULLSCREEN"]);
    until("the window did not go fullscreen", || x.size(full) == xwm::SCREEN);
    let menu = x.window();
    x.set_atoms(menu, "_NET_WM_WINDOW_TYPE", &["_NET_WM_WINDOW_TYPE_POPUP_MENU"]);
    x.map_managed(menu);
    until("the menu is not above the fullscreen window", || x.stacking_order(&[full, menu]) == vec![full, menu]);
}
