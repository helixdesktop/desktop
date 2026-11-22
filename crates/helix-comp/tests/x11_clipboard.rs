//! An X11 app's clipboard reaches Wayland apps in every type it offers,
//! not only plain text: an image copied in Krita (X11 through Xwayland)
//! pasted into a GTK 4 app. Smithay 0.7.0 looked a type's atom up in a
//! property that was never there, so every type but text failed
//! (vendor/smithay, `send_selection`).
//!
//! Runs the real `helix-comp` headless; needs `dbus-daemon` and Xwayland
//! (as every headless session), skipped otherwise.

mod common;

use std::time::Duration;

use helix_test_support::headless::{HeadlessSession, Options};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, CreateWindowAux, EventMask, PropMode, SelectionNotifyEvent, WindowClass, SELECTION_NOTIFY_EVENT};
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;

/// Not a real PNG; any bytes do, as long as they come back unchanged.
fn picture() -> Vec<u8> {
    (0..10_000u32).map(|i| (i * 7 % 251) as u8).collect()
}

/// An X11 client owning CLIPBOARD as `image/png` and plain text, answering
/// requests until the connection closes.
fn x11_owner(session: &HeadlessSession) {
    std::env::set_var("XAUTHORITY", &session.x_authority);
    let (conn, screen_num) = x11rb::connect(Some(&session.x_display)).expect("connect to Xwayland");
    let root = conn.setup().roots[screen_num].root;
    let window = conn.generate_id().unwrap();
    conn.create_window(0, window, root, 0, 0, 1, 1, 0, WindowClass::INPUT_ONLY, 0, &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE)).unwrap();
    let atom = |name: &str| conn.intern_atom(false, name.as_bytes()).unwrap().reply().unwrap().atom;
    let (clipboard, targets, png, utf8) = (atom("CLIPBOARD"), atom("TARGETS"), atom("image/png"), atom("UTF8_STRING"));
    conn.set_selection_owner(window, clipboard, x11rb::CURRENT_TIME).unwrap();
    conn.flush().unwrap();
    std::thread::spawn(move || {
        while let Ok(event) = conn.wait_for_event() {
            let Event::SelectionRequest(request) = event else { continue };
            let property = if request.target == targets {
                conn.change_property32(PropMode::REPLACE, request.requestor, request.property, AtomEnum::ATOM, &[targets, png, utf8]).unwrap();
                request.property
            } else if request.target == png {
                conn.change_property8(PropMode::REPLACE, request.requestor, request.property, png, &picture()).unwrap();
                request.property
            } else if request.target == utf8 {
                conn.change_property8(PropMode::REPLACE, request.requestor, request.property, utf8, b"words").unwrap();
                request.property
            } else {
                x11rb::NONE
            };
            let notify = SelectionNotifyEvent {
                response_type: SELECTION_NOTIFY_EVENT,
                sequence: 0,
                time: request.time,
                requestor: request.requestor,
                selection: request.selection,
                target: request.target,
                property,
            };
            conn.send_event(false, request.requestor, EventMask::NO_EVENT, notify).unwrap();
            conn.flush().unwrap();
        }
    });
}

#[test]
fn an_x11_image_pastes_into_a_wayland_app() {
    let comp = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-comp"));
    let Some(session) = HeadlessSession::start(Options { comp: &comp, shell: None, size: (800, 600), settings: "", env: Vec::new() }) else { return };
    x11_owner(&session);
    assert!(session.wait_for_log("selection set by an X11 app", Duration::from_secs(10)), "the compositor never saw the X11 copy");

    // A focused Wayland window is offered the selection.
    let mut app = common::connect(&session);
    let (window, ()) = app.toplevel((200, 100), |_, _| {});
    let surface = window.surface.clone();
    app.until("the window has no focus", |s| s.keyboard.as_ref() == Some(&surface));
    app.until("no clipboard offered", |s| s.selection.is_some());
    let types = app.state.selection.as_ref().unwrap().1.clone();
    assert!(types.iter().any(|t| t == "image/png"), "offered {types:?}");

    assert_eq!(app.paste("text/plain;charset=utf-8"), b"words");
    let image = app.paste("image/png");
    assert!(image == picture(), "the image arrived whole: {} of {} bytes", image.len(), picture().len());
}
