//! A window's dialogs (non-modal too: JetBrains Find, GIMP and Krita tool
//! dialogs, Wine owned windows) stay above it when it is raised, as in
//! mutter (#225). X11 dialogs under either window manager (`HELIX_XWM`),
//! and Wayland ones (xdg_toplevel.set_parent).

mod common;

use common::xwm::{session_with_shell, until, X};
use x11rb::protocol::xproto::AtomEnum;

/// Activating an X11 window keeps its transient window above it.
#[test]
fn an_x11_dialog_stays_above_its_window_when_the_window_is_raised() {
    let Some((session, mut shell)) = session_with_shell("transient-x11") else { return };
    let x = X::new(&session);
    let parent = x.mapped_titled("parent");
    let dialog = x.window();
    x.set32(dialog, "WM_TRANSIENT_FOR", AtomEnum::WINDOW, &[parent]);
    x.set8(dialog, "_NET_WM_NAME", x.atom("UTF8_STRING"), b"dialog");
    x.map_managed(dialog);
    until("the dialog is not above its window", || x.stacking_order(&[parent, dialog]) == [parent, dialog]);
    let id = shell.window("parent").id;
    shell.activate(id);
    shell.until("the window did not get focus", |s| s.focused == Some(id));
    until("the dialog went under its window", || x.stacking_order(&[parent, dialog]) == [parent, dialog]);
}

/// Activating a Wayland window keeps its (non-modal) child above it:
/// where they overlap, the pointer is on the child.
#[test]
fn a_wayland_dialog_stays_above_its_window_when_the_window_is_raised() {
    let Some((mut session, mut shell)) = session_with_shell("transient-wayland") else { return };
    let mut app = common::connect(&session);
    let (parent, ()) = app.toplevel((400, 300), |_, t| t.toplevel.set_title("parent".into()));
    let (dialog, ()) = app.toplevel((200, 150), |_, t| {
        t.toplevel.set_title("dialog".into());
        t.toplevel.set_parent(Some(&parent.toplevel));
    });
    // Placed once it has a size (its first buffer): listed before that
    // with none.
    let geometry = |shell: &mut common::xwm::Shell, title: &str| {
        let placed = |w: &common::xwm::Listed| w.title == title && w.geometry.is_some_and(|(_, _, w, h)| w > 0 && h > 0);
        shell.until(&format!("{title} has no place"), |s| s.windows.iter().any(placed));
        shell.state.windows.iter().find(|w| placed(w)).and_then(|w| w.geometry).unwrap()
    };
    let (px, py, pw, ph) = geometry(&mut shell, "parent");
    let (dx, dy, dw, dh) = geometry(&mut shell, "dialog");
    // A point on both.
    let (cx, cy) = ((px.max(dx) + (px + pw).min(dx + dw)) / 2, (py.max(dy) + (py + ph).min(dy + dh)) / 2);
    assert!(cx > px.max(dx) && cy > py.max(dy), "the dialog doesn't overlap its window: {:?} {:?}", (px, py, pw, ph), (dx, dy, dw, dh));
    let id = shell.window("parent").id;
    shell.activate(id);
    shell.until("the window did not get focus", |s| s.focused == Some(id));
    session.input("move 1 1");
    session.input(&format!("move {cx} {cy}"));
    let child = dialog.surface.clone();
    app.until("the pointer is not on the dialog above its window", |s| s.pointer.as_ref() == Some(&child));
}
