//! An X11 app's clipboard reaches Wayland apps in every type it offers,
//! not only plain text: an image copied in Krita (X11 through Xwayland)
//! pasted into a GTK 4 app. Smithay 0.7.0 looked a type's atom up in a
//! property that was never there, so every type but text failed
//! (vendor/smithay, `send_selection`); and large ones, sent
//! incrementally, came with 4 bytes of size hint in front (#161).
//!
//! And an X11 client reads the clipboard whatever has focus, as in GNOME:
//! `xclip -o` run from a Wayland terminal (#172), and as often as it
//! asks: an app pasting again with the same window (#160).
//!
//! Runs the real `helix-comp` headless; needs `dbus-daemon` and Xwayland
//! (as every headless session), skipped otherwise.

mod common;

use std::time::Duration;

use helix_test_support::headless::{HeadlessSession, Options};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ChangeWindowAttributesAux, ConnectionExt, CreateWindowAux, Property, EventMask, PropMode, SelectionNotifyEvent, WindowClass, SELECTION_NOTIFY_EVENT};
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;

/// Not a real PNG; any bytes do, as long as they come back unchanged.
fn picture(len: u32) -> Vec<u8> {
    (0..len).map(|i| (i * 7 % 251) as u8).collect()
}

/// Larger data goes incrementally (INCR), in chunks of this size, as
/// Xlib apps do above their maximum request size.
const CHUNK: usize = 64 * 1024;

/// An X11 client owning CLIPBOARD as `image/png` (`image`) and plain text,
/// answering requests until the connection closes.
fn x11_owner(session: &HeadlessSession, image: Vec<u8>) {
    let (conn, screen_num) = common::x11(session);
    let root = conn.setup().roots[screen_num].root;
    let window = conn.generate_id().unwrap();
    conn.create_window(0, window, root, 0, 0, 1, 1, 0, WindowClass::INPUT_ONLY, 0, &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE)).unwrap();
    let atom = |name: &str| conn.intern_atom(false, name.as_bytes()).unwrap().reply().unwrap().atom;
    let (clipboard, targets, png, utf8, incr) = (atom("CLIPBOARD"), atom("TARGETS"), atom("image/png"), atom("UTF8_STRING"), atom("INCR"));
    conn.set_selection_owner(window, clipboard, x11rb::CURRENT_TIME).unwrap();
    conn.flush().unwrap();
    std::thread::spawn(move || {
        // Incremental transfers under way: (requestor, property) and what
        // is left to send.
        let mut sending: Vec<((u32, u32), Vec<u8>)> = Vec::new();
        while let Ok(event) = conn.wait_for_event() {
            if let Event::PropertyNotify(n) = event {
                // The requestor took the last chunk: send the next (an
                // empty one ends the transfer).
                let Some(i) = sending.iter().position(|(k, _)| *k == (n.window, n.atom)) else { continue };
                if n.state != Property::DELETE {
                    continue;
                }
                let left = &mut sending[i].1;
                let chunk: Vec<u8> = left.drain(..left.len().min(CHUNK)).collect();
                conn.change_property8(PropMode::REPLACE, n.window, n.atom, png, &chunk).unwrap();
                if chunk.is_empty() {
                    sending.remove(i);
                }
                conn.flush().unwrap();
                continue;
            }
            let Event::SelectionRequest(request) = event else { continue };
            let property = if request.target == targets {
                conn.change_property32(PropMode::REPLACE, request.requestor, request.property, AtomEnum::ATOM, &[targets, png, utf8]).unwrap();
                request.property
            } else if request.target == png && image.len() > CHUNK {
                conn.change_window_attributes(request.requestor, &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE)).unwrap();
                conn.change_property32(PropMode::REPLACE, request.requestor, request.property, incr, &[image.len() as u32]).unwrap();
                sending.push(((request.requestor, request.property), image.clone()));
                request.property
            } else if request.target == png {
                conn.change_property8(PropMode::REPLACE, request.requestor, request.property, png, &image).unwrap();
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

/// An X11 app copies an image of `len` bytes; a focused Wayland app
/// pastes it (and the text).
fn x11_copy_wayland_pastes(len: u32) {
    let comp = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-comp"));
    let Some(session) = HeadlessSession::start(Options { comp: &comp, shell: None, size: (800, 600), settings: "", env: Vec::new() }) else { return };
    if session.x_display.is_empty() {
        eprintln!("no Xwayland; skipped");
        return;
    }
    x11_owner(&session, picture(len));
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
    let first = |d: &[u8]| d.iter().take(8).copied().collect::<Vec<u8>>();
    assert!(image == picture(len), "the image arrived whole: {} of {len} bytes, starting {:?}", image.len(), first(&image));
}

#[test]
fn an_x11_image_pastes_into_a_wayland_app() {
    x11_copy_wayland_pastes(10_000);
}

/// Over the chunk size the image goes incrementally; the INCR size hint
/// is not data (#161).
#[test]
fn a_large_x11_image_pastes_whole() {
    x11_copy_wayland_pastes(400_000);
}

/// The CLIPBOARD's `target`, read `times` over by one X11 client that has
/// no focused window (as `xclip -o`, or an app pasting again); `None`
/// where refused.
fn x11_reads(session: &HeadlessSession, target: &str, times: usize) -> Vec<Option<Vec<u8>>> {
    let (conn, screen_num) = common::x11(session);
    let root = conn.setup().roots[screen_num].root;
    let window = conn.generate_id().unwrap();
    conn.create_window(0, window, root, 0, 0, 1, 1, 0, WindowClass::INPUT_ONLY, 0, &CreateWindowAux::new()).unwrap();
    let atom = |name: &str| conn.intern_atom(false, name.as_bytes()).unwrap().reply().unwrap().atom;
    let (clipboard, target, property) = (atom("CLIPBOARD"), atom(target), atom("HELIX_TEST"));
    // The compositor takes the X11 selection a moment after the Wayland copy.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while conn.get_selection_owner(clipboard).unwrap().reply().unwrap().owner == x11rb::NONE {
        assert!(std::time::Instant::now() < deadline, "the compositor never took the X11 clipboard");
        std::thread::sleep(Duration::from_millis(10));
    }
    let read = || {
        conn.convert_selection(window, clipboard, target, property, x11rb::CURRENT_TIME).unwrap();
        conn.flush().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            assert!(std::time::Instant::now() < deadline, "no answer to the X11 clipboard request");
            let Some(event) = conn.poll_for_event().unwrap() else {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            };
            let Event::SelectionNotify(notify) = event else { continue };
            if notify.property == x11rb::NONE {
                return None;
            }
            let reply = conn.get_property(true, window, property, AtomEnum::ANY, 0, u32::MAX / 4).unwrap().reply().unwrap();
            return Some(reply.value);
        }
    };
    (0..times).map(|_| read()).collect()
}

/// A focused Wayland app copies `text`; an X11 client reads it `times`.
fn wayland_copy_x11_reads(session: &HeadlessSession, text: &[u8], times: usize) -> Vec<Option<Vec<u8>>> {
    let mut app = common::connect(session);
    let (window, ()) = app.toplevel((200, 100), |_, _| {});
    let surface = window.surface.clone();
    app.until("the window has no focus", |s| s.keyboard.as_ref() == Some(&surface));
    app.copy("text/plain;charset=utf-8", text);
    // Read on another thread: the Wayland app answers on this one.
    std::thread::scope(|scope| {
        let reading = scope.spawn(|| x11_reads(session, "UTF8_STRING", times));
        while !reading.is_finished() {
            app.roundtrip();
            std::thread::sleep(Duration::from_millis(10));
        }
        reading.join().unwrap()
    })
}

#[test]
fn an_x11_client_reads_the_clipboard_without_focus() {
    let comp = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-comp"));
    let Some(session) = HeadlessSession::start(Options { comp: &comp, shell: None, size: (800, 600), settings: "", env: Vec::new() }) else { return };
    if session.x_display.is_empty() {
        eprintln!("no Xwayland; skipped");
        return;
    }
    // No X11 window exists, let alone has focus.
    assert_eq!(wayland_copy_x11_reads(&session, b"from wayland", 1), [Some(b"from wayland".to_vec())]);
}

/// GTK 3, Qt and Java apps paste with one window over and over (#160).
#[test]
fn an_x11_app_pastes_more_than_once() {
    let comp = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-comp"));
    let Some(session) = HeadlessSession::start(Options { comp: &comp, shell: None, size: (800, 600), settings: "", env: Vec::new() }) else { return };
    if session.x_display.is_empty() {
        eprintln!("no Xwayland; skipped");
        return;
    }
    let text = b"again".to_vec();
    assert_eq!(wayland_copy_x11_reads(&session, &text, 3), [Some(text.clone()), Some(text.clone()), Some(text)]);
}
