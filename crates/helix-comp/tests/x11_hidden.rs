//! Hidden X11 windows (minimized, on another workspace) are stacked below
//! the shown ones on the X server (#232): X routes drags between X11 apps
//! and answers XQueryPointer by its own stack, so a hidden window left
//! above a shown one would get them. Under either window manager
//! (`HELIX_XWM`).
//!
//! Three windows, bottom to top: the middle one is hidden. Focus goes to
//! the top one, so nothing else moves the middle one below the bottom one.

mod common;

use common::xwm::{action, session_with_shell, until, X};

/// A minimized window goes below the shown ones, and back on top when
/// shown again.
#[test]
fn a_minimized_x11_window_goes_under_the_shown_ones() {
    let Some((session, mut shell)) = session_with_shell("hidden-minimized") else { return };
    let x = X::new(&session);
    let bottom = x.mapped_titled("bottom");
    let middle = x.mapped_titled("middle");
    let top = x.mapped_titled("top");
    until("the windows are not stacked as mapped", || x.stacking_order(&[bottom, middle, top]) == [bottom, middle, top]);
    let id = shell.window("middle").id;
    shell.action(id, action::MINIMIZE);
    until("the minimized window is still above a shown one", || x.stacking_order(&[bottom, middle, top]) == [middle, bottom, top]);
    shell.activate(id);
    until("the window shown again is not on top", || x.stacking_order(&[bottom, middle, top]) == [bottom, top, middle]);
}

/// A window put on another workspace goes below the shown ones; switching
/// to that workspace stacks it above the ones hidden then.
#[test]
fn an_x11_window_on_another_workspace_goes_under_the_shown_ones() {
    let Some((session, mut shell)) = session_with_shell("hidden-parked") else { return };
    let x = X::new(&session);
    let bottom = x.mapped_titled("bottom");
    let middle = x.mapped_titled("middle");
    let top = x.mapped_titled("top");
    until("the windows are not stacked as mapped", || x.stacking_order(&[bottom, middle, top]) == [bottom, middle, top]);
    let id = shell.window("middle").id;
    shell.move_window(id, 1);
    until("the window on another workspace is still above a shown one", || x.stacking_order(&[bottom, middle, top]) == [middle, bottom, top]);
    shell.switch_workspace(1);
    until("the window shown on its workspace is not above the hidden ones", || {
        let order = x.stacking_order(&[bottom, middle, top]);
        order.last() == Some(&middle)
    });
}
