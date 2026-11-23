//! The workspace an X11 window asks to start on (#279): `_NET_WM_DESKTOP`
//! set before mapping (an app, or a session restore, opening a window on
//! a given workspace), or asked for by client message before mapping. As
//! mutter: a workspace there is puts the window there, hidden until that
//! workspace is shown; 0xFFFFFFFF puts it on every workspace; any other
//! number leaves it on the active one. The window manager then keeps the
//! property saying where the window is, and removes it when the window is
//! withdrawn (EWMH).
//!
//! Four static workspaces, so that the numbers don't depend on which are
//! in use.

mod common;

use common::xwm::{session_settings_env, until, X};
use common::Xwm;
use helix_test_support::headless::HeadlessSession;
use x11rb::protocol::xproto::{AtomEnum, Window};

const WHY: &str = "Smithay's window manager ignores the workspace a window asks to start on (#279)";

/// Every workspace (EWMH).
const ALL: u32 = 0xFFFF_FFFF;

fn session() -> Option<HeadlessSession> {
    session_settings_env("[org/gnome/mutter]\ndynamic-workspaces=false\n\n[org/gnome/desktop/wm/preferences]\nnum-workspaces=4\n", Vec::new())
}

fn desktop(x: &X, window: Window) -> Option<Vec<u32>> {
    x.get32(window, "_NET_WM_DESKTOP")
}

fn current(x: &X) -> Vec<u32> {
    x.get32(x.root, "_NET_CURRENT_DESKTOP").unwrap_or_default()
}

/// Ask for workspace `n`, as a pager does.
fn switch(x: &X, n: u32) {
    x.tell_wm(x.root, "_NET_CURRENT_DESKTOP", [n, x11rb::CURRENT_TIME, 0, 0, 0]);
    until("the workspace asked for never became the current one", || current(x) == [n]);
}

/// A window shown on the first workspace (focused), and a session with
/// four workspaces.
fn shown_one(x: &X) -> Window {
    until("there are not four workspaces", || x.get32(x.root, "_NET_NUMBER_OF_DESKTOPS") == Some(vec![4]));
    let shown = x.mapped();
    until("the first window is not active", || x.active() == Some(shown));
    assert_eq!(current(x), [0]);
    shown
}

/// `window` is on workspace 2, not the current one: it is below the shown
/// window and not focused; switching to workspace 2 shows and focuses it.
fn on_the_third_workspace(x: &X, shown: Window, window: Window) {
    until("the window is not said to be on the workspace it asked for", || desktop(x, window) == Some(vec![2]));
    x.sync();
    assert_eq!(current(x), [0], "the workspace changed with a window opening on another one");
    assert_eq!(x.stacking_order(&[shown, window]), [window, shown], "the window on another workspace is above the shown one");
    assert_eq!(x.active(), Some(shown), "the window on another workspace took the focus");
    switch(x, 2);
    until("the window is not shown on its workspace", || x.stacking_order(&[shown, window]) == [shown, window]);
    until("the window shown on its workspace is not focused", || x.active() == Some(window));
}

/// `_NET_WM_DESKTOP` set before mapping: the window opens on that
/// workspace, hidden until it is shown. Moved back by message once shown
/// (handled already), and the property goes when it is withdrawn.
#[test]
fn the_property_set_before_mapping_puts_it_on_that_workspace() {
    if common::known_failure(&[Xwm::Smithay], WHY) {
        return;
    }
    let Some(session) = session() else { return };
    let x = X::new(&session);
    let shown = shown_one(&x);
    let window = x.window();
    x.set32(window, "_NET_WM_DESKTOP", AtomEnum::CARDINAL, &[2]);
    x.map_managed(window);
    on_the_third_workspace(&x, shown, window);
    // Asked to go back to the first workspace, shown there.
    x.tell_wm(window, "_NET_WM_DESKTOP", [0, 1, 0, 0, 0]);
    until("the window asked to go to the first workspace is not said to be there", || desktop(&x, window) == Some(vec![0]));
    switch(&x, 0);
    until("the window moved to the first workspace is not shown there", || x.stacking_order(&[shown, window]) == [shown, window]);
    x.withdraw(window);
    until("the window was not withdrawn", || x.wm_state(window) == Some(0));
    until("_NET_WM_DESKTOP stays on a withdrawn window", || desktop(&x, window).is_none());
}

/// A `_NET_WM_DESKTOP` message before mapping: the window opens on that
/// workspace, over what the property says.
#[test]
fn a_message_before_mapping_puts_it_on_that_workspace() {
    if common::known_failure(&[Xwm::Smithay], WHY) {
        return;
    }
    let Some(session) = session() else { return };
    let x = X::new(&session);
    let shown = shown_one(&x);
    let window = x.window();
    x.set32(window, "_NET_WM_DESKTOP", AtomEnum::CARDINAL, &[1]);
    x.tell_wm(window, "_NET_WM_DESKTOP", [2, 1, 0, 0, 0]);
    x.map_managed(window);
    on_the_third_workspace(&x, shown, window);
}

/// 0xFFFFFFFF: on every workspace, so still shown after switching.
#[test]
fn all_workspaces_before_mapping_puts_it_on_every_one() {
    if common::known_failure(&[Xwm::Smithay], WHY) {
        return;
    }
    let Some(session) = session() else { return };
    let x = X::new(&session);
    let shown = shown_one(&x);
    let window = x.window();
    x.set32(window, "_NET_WM_DESKTOP", AtomEnum::CARDINAL, &[ALL]);
    x.map_managed(window);
    until("the window is not said to be on every workspace", || desktop(&x, window) == Some(vec![ALL]));
    until("the window on every workspace is not focused", || x.active() == Some(window));
    switch(&x, 1);
    x.sync();
    assert_eq!(x.stacking_order(&[shown, window]), [shown, window], "the window on every workspace went under one on another workspace");
    assert_eq!(desktop(&x, window), Some(vec![ALL]), "the window is no longer on every workspace");
    assert_eq!(x.active(), Some(window), "the window on every workspace lost the focus on switching");
}

/// A workspace there isn't: the window opens on the current one (here the
/// second), and the property says so. One there is, asked for next to it,
/// still counts.
#[test]
fn a_workspace_out_of_range_leaves_it_on_the_current_one() {
    if common::known_failure(&[Xwm::Smithay], WHY) {
        return;
    }
    let Some(session) = session() else { return };
    let x = X::new(&session);
    let shown = shown_one(&x);
    switch(&x, 1);
    for asked in [4, 1000] {
        let window = x.window();
        x.set32(window, "_NET_WM_DESKTOP", AtomEnum::CARDINAL, &[asked]);
        x.map_managed(window);
        until("a window asking for a workspace there isn't is not on the current one", || desktop(&x, window) == Some(vec![1]));
        until("a window asking for a workspace there isn't is not focused", || x.active() == Some(window));
        assert_eq!(current(&x), [1]);
    }
    let window = x.window();
    x.tell_wm(window, "_NET_WM_DESKTOP", [7, 1, 0, 0, 0]);
    x.set32(window, "_NET_WM_DESKTOP", AtomEnum::CARDINAL, &[0]);
    x.map_managed(window);
    until("a window asking by message for a workspace there isn't is not on the current one", || desktop(&x, window) == Some(vec![1]));
    // In range: on the first workspace, with the first window.
    let window = x.window();
    x.set32(window, "_NET_WM_DESKTOP", AtomEnum::CARDINAL, &[0]);
    x.map_managed(window);
    until("a window asking for the first workspace is not there", || desktop(&x, window) == Some(vec![0]));
    switch(&x, 0);
    until("the window is not shown on the first workspace", || x.stacking_order(&[shown, window]) == [shown, window]);
}
