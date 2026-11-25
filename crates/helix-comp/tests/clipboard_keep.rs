//! The clipboard stays after the app that copied exits (#284), as in
//! GNOME: Helix keeps a copy of each new clipboard (its text and its
//! image, within limits) and, when the app goes away, offers that copy to
//! Wayland and X11 apps itself. X11 apps find a clipboard manager
//! (`CLIPBOARD_MANAGER`) that answers `SAVE_TARGETS`. Copies marked secret
//! (`x-kde-passwordManagerHint`) and clipboards cleared on purpose aren't
//! kept. And an X11 app taking the clipboard that never says what it
//! offers (`TARGETS` unanswered) doesn't leave the last copy offered to
//! Wayland apps (#270). Catalog: `x11_cases.md` X134, X142.
//!
//! Runs the real `helix-comp` headless; needs `dbus-daemon` and Xwayland
//! (as every headless session), skipped otherwise.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::xwm;
use helix_test_support::headless::HeadlessSession;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, CreateWindowAux, EventMask, PropMode, SelectionNotifyEvent, Window, WindowClass, SELECTION_NOTIFY_EVENT};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

const TEXT: &str = "text/plain;charset=utf-8";
const HINT: &str = "x-kde-passwordManagerHint";

/// Bytes that come back unchanged or not.
fn data(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u32 * 7 % 251) as u8 ^ seed).collect()
}

fn atom(conn: &RustConnection, name: &str) -> u32 {
    conn.intern_atom(false, name.as_bytes()).unwrap().reply().unwrap().atom
}

fn helper_window(conn: &RustConnection, root: Window) -> Window {
    let window = conn.generate_id().unwrap();
    conn.create_window(0, window, root, 0, 0, 1, 1, 0, WindowClass::INPUT_ONLY, 0, &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE)).unwrap();
    window
}

/// How many times the log says `text` so far.
fn logged(session: &HeadlessSession, text: &str) -> usize {
    session.log().matches(text).count()
}

/// Run until the log says `text` more than `before` times, `apps`
/// answering meanwhile (a Wayland app writes its copy when it reads its
/// events).
fn until_logged(session: &HeadlessSession, apps: &mut [&mut common::Client], text: &str, before: usize) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while logged(session, text) <= before {
        assert!(Instant::now() < deadline, "the log never said \"{text}\"");
        for app in apps.iter_mut() {
            app.roundtrip();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Roundtrips for `time`, then whether `app` is offered a clipboard.
fn offered_after(app: &mut common::Client, time: Duration) -> bool {
    let until = Instant::now() + time;
    while Instant::now() < until {
        app.roundtrip();
        std::thread::sleep(Duration::from_millis(20));
    }
    app.roundtrip();
    app.state.selection.is_some()
}

/// A Wayland app with a focused window.
fn wayland_app(session: &HeadlessSession) -> (common::Client, common::Toplevel) {
    let mut app = common::connect(session);
    let (window, ()) = app.toplevel((200, 100), |_, _| {});
    let surface = window.surface.clone();
    app.until("the Wayland window has no focus", |s| s.keyboard.as_ref() == Some(&surface));
    (app, window)
}

/// A Wayland app copies `types`, Helix keeps it, and the app exits.
fn wayland_copies_and_exits(session: &HeadlessSession, types: &[(&str, &[u8])], wait_for: &str) {
    let before = logged(session, wait_for);
    let (mut app, window) = wayland_app(session);
    app.copy_types(types);
    until_logged(session, &mut [&mut app], wait_for, before);
    drop(window);
    drop(app);
}

/// The text offered and pasted by a new focused Wayland app once Helix
/// took the clipboard over (`None` when nothing is offered after a while).
fn pasted_after_exit(session: &HeadlessSession, mime: &str) -> Option<Vec<u8>> {
    let (mut app, _window) = wayland_app(session);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        app.roundtrip();
        if app.state.selection.as_ref().is_some_and(|(_, types)| types.iter().any(|t| t == mime)) {
            return Some(app.paste(mime));
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// An X11 app owning CLIPBOARD with `offers` (target name, data), as
/// xterm does, until dropped: then its connection closes (the app exits).
struct X11Owner {
    stop: Arc<AtomicBool>,
    clear: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl X11Owner {
    fn new(session: &HeadlessSession, offers: Vec<(&'static str, Vec<u8>)>, answer: bool) -> Self {
        let (conn, screen_num) = common::x11(session);
        let root = conn.setup().roots[screen_num].root;
        let window = helper_window(&conn, root);
        let (clipboard, targets) = (atom(&conn, "CLIPBOARD"), atom(&conn, "TARGETS"));
        let offers: Vec<(u32, Vec<u8>)> = offers.into_iter().map(|(name, data)| (atom(&conn, name), data)).collect();
        conn.set_selection_owner(window, clipboard, x11rb::CURRENT_TIME).unwrap();
        conn.flush().unwrap();
        let (stop, clear) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let (stopping, clearing) = (stop.clone(), clear.clone());
        let thread = std::thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                let Ok(event) = conn.poll_for_event() else { return };
                let Some(event) = event else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let Event::SelectionRequest(request) = event else { continue };
                if !answer {
                    continue;
                }
                let property = if request.target == targets {
                    let mut list = vec![targets];
                    list.extend(offers.iter().map(|(a, _)| *a));
                    conn.change_property32(PropMode::REPLACE, request.requestor, request.property, AtomEnum::ATOM, &list).unwrap();
                    request.property
                } else if let Some((kind, bytes)) = offers.iter().find(|(a, _)| *a == request.target) {
                    conn.change_property8(PropMode::REPLACE, request.requestor, request.property, *kind, bytes).unwrap();
                    request.property
                } else {
                    x11rb::NONE
                };
                let notify = SelectionNotifyEvent { response_type: SELECTION_NOTIFY_EVENT, sequence: 0, time: request.time, requestor: request.requestor, selection: request.selection, target: request.target, property };
                conn.send_event(false, request.requestor, EventMask::NO_EVENT, notify).unwrap();
                conn.flush().unwrap();
            }
            if clearing.load(Ordering::Relaxed) {
                conn.set_selection_owner(x11rb::NONE, clipboard, x11rb::CURRENT_TIME).unwrap();
                conn.sync().unwrap();
            }
        });
        X11Owner { stop, clear, thread: Some(thread) }
    }

    /// The app clears the clipboard (`SetSelectionOwner(None)`), then exits.
    fn clear_and_exit(self) {
        self.clear.store(true, Ordering::Relaxed);
        self.exit();
    }

    /// The app exits.
    fn exit(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

impl Drop for X11Owner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Wait until someone owns the X CLIPBOARD (the window manager takes it a
/// moment after the Wayland side has a copy).
fn until_x11_owned(conn: &RustConnection, clipboard: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while conn.get_selection_owner(clipboard).unwrap().reply().unwrap().owner == x11rb::NONE {
        assert!(Instant::now() < deadline, "nobody took the X11 clipboard");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// An X11 client reading CLIPBOARD as `target`: `None` when refused.
fn x11_read(session: &HeadlessSession, target: &str) -> Option<Vec<u8>> {
    let (conn, screen_num) = common::x11(session);
    let root = conn.setup().roots[screen_num].root;
    let window = helper_window(&conn, root);
    let (clipboard, target, property) = (atom(&conn, "CLIPBOARD"), atom(&conn, target), atom(&conn, "HELIX_TEST"));
    until_x11_owned(&conn, clipboard);
    conn.convert_selection(window, clipboard, target, property, x11rb::CURRENT_TIME).unwrap();
    conn.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "no answer to the X11 clipboard request");
        let Some(event) = conn.poll_for_event().unwrap() else {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        };
        let Event::SelectionNotify(notify) = event else { continue };
        if notify.property == x11rb::NONE {
            return None;
        }
        return Some(conn.get_property(true, window, property, AtomEnum::ANY, 0, u32::MAX / 4).unwrap().reply().unwrap().value);
    }
}

/// A Wayland app copies text and exits; another Wayland app pastes it.
#[test]
fn a_wayland_copy_pastes_after_its_app_exits() {
    let Some(session) = xwm::session() else { return };
    wayland_copies_and_exits(&session, &[(TEXT, b"kept after exit")], "clipboard kept");
    assert_eq!(pasted_after_exit(&session, TEXT).as_deref(), Some(&b"kept after exit"[..]), "nothing pasted after the app exited");
    // And X11 apps paste it too.
    assert_eq!(x11_read(&session, "UTF8_STRING").as_deref(), Some(&b"kept after exit"[..]));
}

/// #305: a Wayland app destroys its copy's source without clearing the
/// clipboard (`set_selection(NULL)`) and keeps running: Helix takes over
/// the copy it kept, for Wayland apps and for X11 apps (which are no
/// longer offered the gone one).
#[test]
fn a_wayland_copy_whose_source_is_destroyed_pastes_while_its_app_runs() {
    let Some(session) = xwm::session() else { return };
    let kept = logged(&session, "clipboard kept");
    let (mut app, _window) = wayland_app(&session);
    let source = app.copy_types(&[(TEXT, b"source destroyed")]);
    until_logged(&session, &mut [&mut app], "clipboard kept", kept);
    source.destroy();
    app.roundtrip();
    assert_eq!(pasted_after_exit(&session, TEXT).as_deref(), Some(&b"source destroyed"[..]), "nothing pasted after the source was destroyed");
    assert_eq!(x11_read(&session, "UTF8_STRING").as_deref(), Some(&b"source destroyed"[..]), "the X11 app's paste");
    // The app that copied is still there.
    app.roundtrip();
}

/// #305: a Wayland app destroys its primary selection's source and keeps
/// running: X11 apps are no longer offered it (the window manager lets
/// go of PRIMARY).
#[test]
fn a_destroyed_primary_source_is_not_offered_to_x11() {
    let Some(session) = xwm::session() else { return };
    let (mut app, _window) = wayland_app(&session);
    let source = app.copy_primary(TEXT, b"selected");
    let (conn, _) = common::x11(&session);
    let primary = atom(&conn, "PRIMARY");
    until_x11_owned(&conn, primary);
    source.destroy();
    app.roundtrip();
    let deadline = Instant::now() + Duration::from_secs(5);
    while conn.get_selection_owner(primary).unwrap().reply().unwrap().owner != x11rb::NONE {
        assert!(Instant::now() < deadline, "X11 apps are still offered a primary selection whose source is gone");
        app.roundtrip();
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// An image is kept, whole.
#[test]
fn a_copied_image_is_kept() {
    let Some(session) = xwm::session() else { return };
    let picture = data(300_000, 3);
    wayland_copies_and_exits(&session, &[("image/png", &picture)], "clipboard kept");
    let got = pasted_after_exit(&session, "image/png").expect("no image offered after the app exited");
    assert!(got == picture, "{} of {} bytes, not the same", got.len(), picture.len());
}

/// A type over its limit (text: 4 MiB) isn't kept: after the app exits it
/// isn't offered (a paste of it can't hang or come cut), while the image
/// within its limit is.
#[test]
fn a_type_over_its_limit_is_not_kept() {
    let Some(session) = xwm::session() else { return };
    let text = vec![b'a'; 5 * 1024 * 1024];
    let picture = data(1000, 4);
    wayland_copies_and_exits(&session, &[(TEXT, &text), ("image/png", &picture)], "clipboard kept");
    assert!(session.log().contains("too large"), "the text was not found too large");
    let (mut app, _window) = wayland_app(&session);
    app.until("the image was not offered after the app exited", |s| s.selection.as_ref().is_some_and(|(_, t)| t.iter().any(|t| t == "image/png")));
    let types = app.state.selection.as_ref().unwrap().1.clone();
    assert!(!types.iter().any(|t| t.starts_with("text/")), "text offered though not kept: {types:?}");
    assert_eq!(app.paste("image/png"), picture);
    assert_eq!(app.paste(TEXT), b"", "a paste of the text not kept got something");
}

/// A password manager's copy (`x-kde-passwordManagerHint: secret`) goes
/// with its app.
#[test]
fn a_secret_copy_is_not_kept() {
    let Some(session) = xwm::session() else { return };
    wayland_copies_and_exits(&session, &[(TEXT, b"hunter2"), (HINT, b"secret")], "clipboard marked secret");
    let (mut app, _window) = wayland_app(&session);
    assert!(!offered_after(&mut app, Duration::from_millis(1500)), "the secret was kept after its app exited");
}

/// A clipboard an app cleared (`set_selection(NULL)`) stays empty after
/// it exits.
#[test]
fn a_clipboard_cleared_on_purpose_stays_empty() {
    let Some(session) = xwm::session() else { return };
    let kept = logged(&session, "clipboard kept");
    let (mut app, window) = wayland_app(&session);
    app.copy(TEXT, b"cleared later");
    until_logged(&session, &mut [&mut app], "clipboard kept", kept);
    let cleared = logged(&session, "a Wayland app cleared the clipboard");
    app.data_device.set_selection(None, 0);
    until_logged(&session, &mut [&mut app], "a Wayland app cleared the clipboard", cleared);
    drop(window);
    drop(app);
    let (mut other, _window) = wayland_app(&session);
    assert!(!offered_after(&mut other, Duration::from_millis(1500)), "a cleared clipboard came back after its app exited");
}

/// A newer copy replaces the kept one, also once Helix took one over.
#[test]
fn a_new_copy_replaces_the_kept_one() {
    let Some(session) = xwm::session() else { return };
    wayland_copies_and_exits(&session, &[(TEXT, b"first")], "clipboard kept");
    assert_eq!(pasted_after_exit(&session, TEXT).as_deref(), Some(&b"first"[..]));
    wayland_copies_and_exits(&session, &[(TEXT, b"second")], "clipboard kept");
    assert_eq!(pasted_after_exit(&session, TEXT).as_deref(), Some(&b"second"[..]), "the newer copy was not the one kept");
}

/// "Copy in xterm, close it, paste": an X11 app's copy pastes into Wayland
/// apps and X11 apps after it exits.
#[test]
fn an_x11_copy_pastes_after_its_app_exits() {
    let Some(session) = xwm::session() else { return };
    let (mut app, _window) = wayland_app(&session);
    let kept = logged(&session, "clipboard kept");
    let owner = X11Owner::new(&session, vec![("UTF8_STRING", b"from xterm".to_vec()), ("STRING", b"from xterm".to_vec())], true);
    until_logged(&session, &mut [&mut app], "clipboard kept", kept);
    let taken = logged(&session, "Helix keeps its copy");
    owner.exit();
    until_logged(&session, &mut [&mut app], "Helix keeps its copy", taken);
    app.until("the kept copy was not offered", |s| s.selection.as_ref().is_some_and(|(_, t)| t.iter().any(|t| t == TEXT)));
    assert_eq!(app.paste(TEXT), b"from xterm", "the Wayland app's paste");
    assert_eq!(x11_read(&session, "UTF8_STRING").as_deref(), Some(&b"from xterm"[..]), "the X11 app's paste");
}

/// An X11 app clearing the clipboard before it exits: it stays empty.
#[test]
fn an_x11_clipboard_cleared_on_purpose_stays_empty() {
    let Some(session) = xwm::session() else { return };
    let (mut app, _window) = wayland_app(&session);
    let kept = logged(&session, "clipboard kept");
    let owner = X11Owner::new(&session, vec![("UTF8_STRING", b"cleared later".to_vec())], true);
    until_logged(&session, &mut [&mut app], "clipboard kept", kept);
    let cleared = logged(&session, "an X11 app cleared the clipboard");
    owner.clear_and_exit();
    until_logged(&session, &mut [&mut app], "an X11 app cleared the clipboard", cleared);
    assert!(!offered_after(&mut app, Duration::from_millis(1500)), "a cleared X11 clipboard came back after its app exited");
}

/// GTK 3 asks the clipboard manager to save the clipboard as it exits
/// (`SAVE_TARGETS` to `CLIPBOARD_MANAGER`), and waits for the answer: it
/// is answered once the copy is kept, and the copy pastes after.
#[test]
fn save_targets_is_answered() {
    let Some(session) = xwm::session() else { return };
    let (mut app, _window) = wayland_app(&session);
    let owner = X11Owner::new(&session, vec![("UTF8_STRING", b"saved on exit".to_vec())], true);
    let (conn, screen_num) = common::x11(&session);
    let root = conn.setup().roots[screen_num].root;
    let window = helper_window(&conn, root);
    let (manager, save, property, utf8) = (atom(&conn, "CLIPBOARD_MANAGER"), atom(&conn, "SAVE_TARGETS"), atom(&conn, "HELIX_SAVE"), atom(&conn, "UTF8_STRING"));
    assert_ne!(conn.get_selection_owner(manager).unwrap().reply().unwrap().owner, x11rb::NONE, "no clipboard manager");
    // As GTK 3: the targets to save in the property, then the request.
    conn.change_property32(PropMode::REPLACE, window, property, AtomEnum::ATOM, &[utf8]).unwrap();
    conn.convert_selection(window, manager, save, property, x11rb::CURRENT_TIME).unwrap();
    conn.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let answer = loop {
        assert!(Instant::now() < deadline, "SAVE_TARGETS was never answered");
        app.roundtrip();
        match conn.poll_for_event().unwrap() {
            Some(Event::SelectionNotify(n)) if n.target == save => break n,
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    assert_eq!(answer.property, property, "SAVE_TARGETS refused");
    let taken = logged(&session, "Helix keeps its copy");
    owner.exit();
    until_logged(&session, &mut [&mut app], "Helix keeps its copy", taken);
    app.until("the saved copy was not offered", |s| s.selection.as_ref().is_some_and(|(_, t)| t.iter().any(|t| t == TEXT)));
    assert_eq!(app.paste(TEXT), b"saved on exit");
}

/// #270: an X11 app takes the clipboard and never answers `TARGETS`: the
/// Wayland copy before it is no longer offered (it isn't the clipboard),
/// after a few seconds.
#[test]
fn an_x11_owner_never_answering_targets_does_not_leave_the_last_copy_offered() {
    let Some(session) = xwm::session() else { return };
    let (mut app, _window) = wayland_app(&session);
    app.copy(TEXT, b"the copy before");
    app.until("the Wayland copy was not offered", |s| s.selection.is_some());
    // The window manager owns the X clipboard for it before the X11 app
    // takes it.
    let (conn, _) = common::x11(&session);
    until_x11_owned(&conn, atom(&conn, "CLIPBOARD"));
    let _hung = X11Owner::new(&session, vec![("UTF8_STRING", b"never".to_vec())], false);
    let deadline = Instant::now() + Duration::from_secs(10);
    while app.state.selection.is_some() {
        assert!(Instant::now() < deadline, "the last copy is still offered while an X11 app that never answers owns the clipboard");
        app.roundtrip();
        std::thread::sleep(Duration::from_millis(50));
    }
}
