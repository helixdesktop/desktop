//! A new X11 window that gets focus gets the X focus and the keyboard,
//! with no FocusOut while it gets its surface (#229: Wine and SDL games
//! may take a FocusOut as being deactivated). Under either window manager
//! (`HELIX_XWM`).

mod common;

use std::time::Duration;

use common::xwm::{session, until, X};
use x11rb::protocol::xproto::{ChangeWindowAttributesAux, ConnectionExt as _, EventMask, NotifyDetail, NotifyMode};
use x11rb::protocol::Event;

/// Map a window (fullscreen before it is shown, or not) and check it gets
/// the X focus, no FocusOut, and keys.
fn focus_at_map(fullscreen: bool) {
    let Some(mut session) = session() else { return };
    let x = X::new(&session);
    let window = x.window();
    x.conn.change_window_attributes(window, &ChangeWindowAttributesAux::new().event_mask(EventMask::FOCUS_CHANGE | EventMask::KEY_PRESS)).unwrap();
    if fullscreen {
        x.set_atoms(window, "_NET_WM_STATE", &["_NET_WM_STATE_FULLSCREEN"]);
    }
    x.map_managed(window);
    until("the new window never got the X focus", || x.input_focus() == window);
    let events = x.events_for(Duration::from_secs(1));
    // Detail Pointer events only follow the pointer while the X focus is
    // PointerRoot (ICCCM: not a change of focus; apps ignore them).
    let flaps: Vec<_> = events.iter().filter(|e| matches!(e, Event::FocusOut(f) if f.event == window && f.mode == NotifyMode::NORMAL && f.detail != NotifyDetail::POINTER)).collect();
    assert!(flaps.is_empty(), "the new window lost the focus while it got its surface: {flaps:?}");
    session.input("key 30 1\nkey 30 0");
    x.wait_event("a key never reached the new window", |e| matches!(e, Event::KeyPress(k) if k.event == window).then_some(()));
}

/// Most windows are focused once they have their surface.
#[test]
fn a_new_window_gets_the_focus_and_keeps_it() {
    focus_at_map(false);
}

/// A window fullscreen before it is shown is focused before X maps it
/// (SetInputFocus fails then): it gets the X focus once it has its
/// surface.
#[test]
fn a_window_fullscreen_before_it_is_shown_gets_the_focus() {
    focus_at_map(true);
}
