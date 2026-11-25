//! The clipboard between X11 and Wayland apps: the edge cases of
//! `tests/x11_cases.md` about large (INCR) transfers both ways, repeated
//! and concurrent requests, requestors that give up or go away, owners
//! that never answer, and text targets (#191).
//!
//! Runs the real `helix-comp` headless; needs `dbus-daemon` and Xwayland
//! (as every headless session), skipped otherwise.

mod common;

use std::os::fd::AsFd;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::xwm;
use helix_test_support::headless::HeadlessSession;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ChangeWindowAttributesAux, ConnectionExt, CreateWindowAux, EventMask, PropMode, Property, SelectionNotifyEvent, Window, WindowClass, SELECTION_NOTIFY_EVENT};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// Bytes that come back unchanged or not.
fn data(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u32 * 7 % 251) as u8 ^ seed).collect()
}

/// Above this, transfers go incrementally (INCR) in chunks this big.
const CHUNK: usize = 64 * 1024;

fn atom(conn: &RustConnection, name: &str) -> u32 {
    conn.intern_atom(false, name.as_bytes()).unwrap().reply().unwrap().atom
}

fn helper_window(conn: &RustConnection, root: Window, events: EventMask) -> Window {
    helper_window_of(conn, root, events, WindowClass::INPUT_ONLY)
}

/// A hidden window of `class` (Xlib apps read the clipboard with an
/// InputOutput one, most often).
fn helper_window_of(conn: &RustConnection, root: Window, events: EventMask, class: WindowClass) -> Window {
    let window = conn.generate_id().unwrap();
    conn.create_window(0, window, root, 0, 0, 1, 1, 0, class, 0, &CreateWindowAux::new().event_mask(events)).unwrap();
    window
}

/// An incremental transfer under way: (requestor, property), the type,
/// what is left to send, and the whole length.
type Sending = ((u32, u32), u32, Vec<u8>, usize);

/// An X11 app owning CLIPBOARD with `offers` (target name, data), answering
/// until the connection closes; or never answering when `stuck`. Each
/// incremental transfer that ends (its empty last chunk sent) is reported.
fn x11_owner(session: &HeadlessSession, offers: Vec<(&'static str, Vec<u8>)>, stuck: bool) -> mpsc::Receiver<usize> {
    x11_owner_of(session, vec![("CLIPBOARD", offers)], stuck)
}

/// What an owner offers: (target name, data).
type Offers = Vec<(&'static str, Vec<u8>)>;

/// The same, as atoms.
type OfferedAs = Vec<(u32, Vec<u8>)>;

/// The same for several selections (each with its own offers), all taken
/// in one go.
fn x11_owner_of(session: &HeadlessSession, selections: Vec<(&'static str, Offers)>, stuck: bool) -> mpsc::Receiver<usize> {
    let (conn, screen_num) = common::x11(session);
    let root = conn.setup().roots[screen_num].root;
    let window = helper_window(&conn, root, EventMask::PROPERTY_CHANGE);
    let (targets, incr) = (atom(&conn, "TARGETS"), atom(&conn, "INCR"));
    let selections: Vec<(u32, OfferedAs)> = selections
        .into_iter()
        .map(|(selection, offers)| (atom(&conn, selection), offers.into_iter().map(|(name, data)| (atom(&conn, name), data)).collect()))
        .collect();
    for (selection, _) in &selections {
        conn.set_selection_owner(window, *selection, x11rb::CURRENT_TIME).unwrap();
    }
    conn.flush().unwrap();
    let (done, finished) = mpsc::channel();
    std::thread::spawn(move || {
        let mut sending: Vec<Sending> = Vec::new();
        while let Ok(event) = conn.wait_for_event() {
            if let Event::PropertyNotify(n) = event {
                let Some(i) = sending.iter().position(|(k, ..)| *k == (n.window, n.atom)) else { continue };
                if n.state != Property::DELETE {
                    continue;
                }
                let (_, kind, left, total) = &mut sending[i];
                let chunk: Vec<u8> = left.drain(..left.len().min(CHUNK)).collect();
                let _ = conn.change_property8(PropMode::REPLACE, n.window, n.atom, *kind, &chunk);
                if chunk.is_empty() {
                    let _ = done.send(*total);
                    sending.remove(i);
                }
                let _ = conn.flush();
                continue;
            }
            let Event::SelectionRequest(request) = event else { continue };
            if stuck {
                continue;
            }
            let offers = selections.iter().find(|(s, _)| *s == request.selection).map_or(&[][..], |(_, o)| o.as_slice());
            let offered = offers.iter().find(|(a, _)| *a == request.target);
            let property = if request.target == targets {
                let mut list = vec![targets];
                list.extend(offers.iter().map(|(a, _)| *a));
                conn.change_property32(PropMode::REPLACE, request.requestor, request.property, AtomEnum::ATOM, &list).unwrap();
                request.property
            } else if let Some((kind, bytes)) = offered {
                if bytes.len() > CHUNK {
                    conn.change_window_attributes(request.requestor, &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE)).unwrap();
                    conn.change_property32(PropMode::REPLACE, request.requestor, request.property, incr, &[bytes.len() as u32]).unwrap();
                    sending.push(((request.requestor, request.property), *kind, bytes.clone(), bytes.len()));
                } else {
                    conn.change_property8(PropMode::REPLACE, request.requestor, request.property, *kind, bytes).unwrap();
                }
                request.property
            } else {
                x11rb::NONE
            };
            let notify = SelectionNotifyEvent { response_type: SELECTION_NOTIFY_EVENT, sequence: 0, time: request.time, requestor: request.requestor, selection: request.selection, target: request.target, property };
            conn.send_event(false, request.requestor, EventMask::NO_EVENT, notify).unwrap();
            conn.flush().unwrap();
        }
    });
    finished
}

/// An X11 app reading CLIPBOARD, incremental transfers included.
struct Reader {
    conn: RustConnection,
    window: Window,
    clipboard: u32,
    incr: u32,
}

impl Reader {
    fn new(session: &HeadlessSession) -> Self {
        Self::with(session, WindowClass::INPUT_OUTPUT)
    }

    /// A reader asking with a window of `class`.
    fn with(session: &HeadlessSession, class: WindowClass) -> Self {
        let (conn, screen_num) = common::x11(session);
        let root = conn.setup().roots[screen_num].root;
        let window = helper_window_of(&conn, root, EventMask::PROPERTY_CHANGE, class);
        let (clipboard, incr) = (atom(&conn, "CLIPBOARD"), atom(&conn, "INCR"));
        let deadline = Instant::now() + Duration::from_secs(5);
        while conn.get_selection_owner(clipboard).unwrap().reply().unwrap().owner == x11rb::NONE {
            assert!(Instant::now() < deadline, "nobody took the X11 clipboard");
            std::thread::sleep(Duration::from_millis(10));
        }
        Reader { conn, window, clipboard, incr }
    }

    fn ask(&self, target: &str, property: &str) {
        let (target, property) = (atom(&self.conn, target), atom(&self.conn, property));
        self.conn.convert_selection(self.window, self.clipboard, target, property, x11rb::CURRENT_TIME).unwrap();
        self.conn.flush().unwrap();
    }

    fn next_event(&self, deadline: Instant, what: &str) -> Event {
        loop {
            assert!(Instant::now() < deadline, "{what}");
            match self.conn.poll_for_event().unwrap() {
                Some(event) => return event,
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    /// The answer to the request just made: `None` when refused. With
    /// `chunks`, also how many chunks it came in, and whether the last
    /// was empty.
    fn answer(&self, deadline: Instant) -> Option<(Vec<u8>, usize)> {
        let notify = loop {
            if let Event::SelectionNotify(n) = self.next_event(deadline, "no answer to the X11 clipboard request") {
                break n;
            }
        };
        if notify.property == x11rb::NONE {
            return None;
        }
        Some(self.take(notify.property, deadline))
    }

    /// What the owner wrote in `property` (deleting it), incrementally
    /// or not, with how many chunks it came in, as [`Reader::answer`].
    fn take(&self, property: u32, deadline: Instant) -> (Vec<u8>, usize) {
        let reply = self.conn.get_property(true, self.window, property, AtomEnum::ANY, 0, u32::MAX / 4).unwrap().reply().unwrap();
        self.conn.flush().unwrap();
        if reply.type_ != self.incr {
            return (reply.value, 1);
        }
        // INCR: each new value is a chunk; an empty one ends it.
        let (mut data, mut chunks) = (Vec::new(), 0);
        loop {
            let Event::PropertyNotify(n) = self.next_event(deadline, "an incremental transfer never ended") else { continue };
            if n.window != self.window || n.atom != property || n.state != Property::NEW_VALUE {
                continue;
            }
            let reply = self.conn.get_property(true, self.window, property, AtomEnum::ANY, 0, u32::MAX / 4).unwrap().reply().unwrap();
            self.conn.flush().unwrap();
            chunks += 1;
            if reply.value.is_empty() {
                return (data, chunks);
            }
            data.extend_from_slice(&reply.value);
        }
    }

    /// Asks for MULTIPLE with `list` (the ATOM_PAIR property's value, as
    /// it is written: type, format and atoms), into HELIX_MULTIPLE. The
    /// list as the owner wrote it back, or `None` when refused.
    fn ask_multiple_raw(&self, type_: u32, format: u8, list: &[u32], deadline: Instant) -> Option<Vec<u32>> {
        let multiple = atom(&self.conn, "HELIX_MULTIPLE");
        match format {
            32 => self.conn.change_property32(PropMode::REPLACE, self.window, multiple, type_, list).unwrap(),
            _ => self.conn.change_property8(PropMode::REPLACE, self.window, multiple, type_, &list.iter().flat_map(|a| a.to_ne_bytes()).collect::<Vec<u8>>()).unwrap(),
        };
        self.ask("MULTIPLE", "HELIX_MULTIPLE");
        let notify = loop {
            if let Event::SelectionNotify(n) = self.next_event(deadline, "a MULTIPLE request was never answered") {
                break n;
            }
        };
        if notify.property == x11rb::NONE {
            return None;
        }
        assert_eq!(notify.property, multiple, "a MULTIPLE request answered in another property");
        let reply = self.conn.get_property(true, self.window, multiple, AtomEnum::ANY, 0, 1024).unwrap().reply().unwrap();
        assert_eq!(reply.type_, atom(&self.conn, "ATOM_PAIR"), "the pairs came back as another type");
        Some(reply.value32().expect("the pairs came back in another format").collect())
    }

    /// Asks for MULTIPLE with `pairs` (target, property names). The
    /// properties as the owner wrote them back (`None` for a pair that
    /// failed), or `None` when the request was refused.
    fn ask_multiple(&self, pairs: &[(&str, &str)], deadline: Instant) -> Option<Vec<Option<u32>>> {
        let list: Vec<u32> = pairs.iter().flat_map(|(t, p)| [atom(&self.conn, t), atom(&self.conn, p)]).collect();
        let back = self.ask_multiple_raw(atom(&self.conn, "ATOM_PAIR"), 32, &list, deadline)?;
        assert_eq!(back.len(), list.len(), "the pairs came back with another length");
        for (sent, got) in list.chunks(2).zip(back.chunks(2)) {
            assert_eq!(sent[0], got[0], "a pair's target changed");
            assert!(got[1] == sent[1] || got[1] == x11rb::NONE, "a pair's property changed to another");
        }
        Some(back.chunks(2).map(|p| (p[1] != x11rb::NONE).then_some(p[1])).collect())
    }

    fn read(&self, target: &str) -> Option<Vec<u8>> {
        self.ask(target, "HELIX_TEST");
        self.answer(Instant::now() + Duration::from_secs(10)).map(|(d, _)| d)
    }
}

/// A focused Wayland app.
fn wayland_app(session: &HeadlessSession) -> common::Client {
    let mut app = common::connect(session);
    let (window, ()) = app.toplevel((200, 100), |_, _| {});
    let surface = window.surface.clone();
    std::mem::forget(window);
    app.until("the Wayland window has no focus", |s| s.keyboard.as_ref() == Some(&surface));
    app
}

/// Run `x11` on another thread while the Wayland app answers on this one.
fn while_serving<T: Send>(app: &mut common::Client, x11: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| {
        let reading = scope.spawn(x11);
        while !reading.is_finished() {
            app.roundtrip();
            std::thread::sleep(Duration::from_millis(5));
        }
        reading.join().unwrap()
    })
}

/// X61: a large Wayland copy reaches an X11 app whole, incrementally,
/// ending with an empty chunk (pasted images were cut or zeroed).
#[test]
fn a_large_wayland_copy_reaches_x11_whole() {
    let Some(session) = xwm::session() else { return };
    let mut app = wayland_app(&session);
    let picture = data(2 * 1024 * 1024 + 123, 1);
    app.copy("image/png", &picture);
    let (got, chunks) = while_serving(&mut app, || {
        let reader = Reader::new(&session);
        reader.ask("image/png", "HELIX_TEST");
        reader.answer(Instant::now() + Duration::from_secs(20)).expect("the X11 app's request was refused")
    });
    assert!(got == picture, "{} of {} bytes, not the same", got.len(), picture.len());
    assert!(chunks > 1, "a 2 MiB copy did not go incrementally");
}

/// X61: the same with an InputOnly window asking (ICCCM: the owner
/// listens for the requestor's property changes itself).
#[test]
fn a_large_wayland_copy_reaches_an_input_only_requestor() {
    let Some(session) = xwm::session() else { return };
    let mut app = wayland_app(&session);
    let picture = data(512 * 1024, 8);
    app.copy("image/png", &picture);
    let got = while_serving(&mut app, || {
        let reader = Reader::with(&session, WindowClass::INPUT_ONLY);
        reader.read("image/png")
    });
    assert!(got.as_ref() == Some(&picture), "{:?} of {} bytes", got.map(|g| g.len()), picture.len());
}

/// X62: small requests in a row get exactly one answer each, never a
/// stray refusal after.
#[test]
fn each_request_gets_one_answer() {
    let Some(session) = xwm::session() else { return };
    let mut app = wayland_app(&session);
    app.copy("text/plain;charset=utf-8", b"once each");
    let stray = while_serving(&mut app, || {
        let reader = Reader::new(&session);
        for _ in 0..3 {
            assert_eq!(reader.read("UTF8_STRING").as_deref(), Some(&b"once each"[..]));
        }
        let end = Instant::now() + Duration::from_secs(1);
        let mut stray = 0;
        while Instant::now() < end {
            match reader.conn.poll_for_event().unwrap() {
                Some(Event::SelectionNotify(_)) => stray += 1,
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        stray
    });
    assert_eq!(stray, 0, "answers nobody asked for");
}

/// X63: a Wayland app pastes from an X11 app again and again.
#[test]
fn a_wayland_app_pastes_an_x11_copy_again_and_again() {
    let Some(session) = xwm::session() else { return };
    x11_owner(&session, vec![("UTF8_STRING", b"from x11".to_vec())], false);
    assert!(session.wait_for_log("selection set by an X11 app", Duration::from_secs(10)), "the compositor never saw the X11 copy");
    let mut app = wayland_app(&session);
    app.until("no clipboard offered", |s| s.selection.is_some());
    for _ in 0..3 {
        assert_eq!(app.paste("text/plain;charset=utf-8"), b"from x11");
    }
}

/// X64: a large X11 copy pasted twice in a row arrives whole both times.
#[test]
fn a_large_x11_copy_pastes_whole_twice() {
    let Some(session) = xwm::session() else { return };
    let picture = data(1024 * 1024 + 7, 2);
    x11_owner(&session, vec![("image/png", picture.clone())], false);
    assert!(session.wait_for_log("selection set by an X11 app", Duration::from_secs(10)), "the compositor never saw the X11 copy");
    let mut app = wayland_app(&session);
    app.until("no clipboard offered", |s| s.selection.is_some());
    for round in 0..2 {
        let got = app.paste("image/png");
        assert!(got == picture, "paste {round}: {} of {} bytes, not the same", got.len(), picture.len());
    }
}

/// #228: a large X11 copy pasted into a Wayland app that reads slowly
/// arrives whole, however long the app takes once the X11 app has sent it
/// all; and the compositor doesn't hold all of it meanwhile: the X11 app
/// is made to wait for the reader.
#[test]
fn a_large_x11_copy_reaches_a_slow_reader_whole() {
    if common::known_failure(&[common::Xwm::Smithay], "Smithay's window manager reads a whole incremental X11 copy ahead of the Wayland reader (#228)") {
        return;
    }
    let Some(session) = xwm::session() else { return };
    let picture = data(16 * 1024 * 1024 + 5, 6);
    let finished = x11_owner(&session, vec![("image/png", picture.clone())], false);
    assert!(session.wait_for_log("selection set by an X11 app", Duration::from_secs(10)), "the compositor never saw the X11 copy");
    // Helix reads the copy first, to keep it (#284): too large to keep,
    // its read ends there and the owner sends the rest to no one. That
    // transfer ends before this test's.
    assert!(session.wait_for_log("too large", Duration::from_secs(30)), "the copy was kept, or never read to keep it");
    assert!(finished.recv_timeout(Duration::from_secs(30)).is_ok(), "the owner never finished the transfer for keeping the copy");
    let mut app = wayland_app(&session);
    app.until("no clipboard offered", |s| s.selection.is_some());
    let (offer, _) = app.state.selection.clone().unwrap();
    let (read, write) = std::io::pipe().unwrap();
    offer.receive("image/png".to_owned(), write.as_fd());
    drop(write);
    app.roundtrip();
    // A few MB a second: well past the 5 s a stalled transfer is given.
    let (progress, read_so_far) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut got = Vec::new();
        let mut chunk = vec![0u8; 192 * 1024];
        loop {
            match std::io::Read::read(&mut { &read }, &mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    got.extend_from_slice(&chunk[..n]);
                    let _ = progress.send(got.len());
                }
            }
            std::thread::sleep(Duration::from_millis(64));
        }
        got
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut checked = false;
    while !reader.is_finished() {
        assert!(Instant::now() < deadline, "the paste never ended");
        app.roundtrip();
        if !checked && read_so_far.try_iter().last().is_some_and(|n| n >= 2 * 1024 * 1024) {
            // 2 MB in: the X11 app can't have sent all 16 MB already.
            assert!(finished.try_recv().is_err(), "the X11 app sent all of it while the reader had 2 MB: the compositor buffers the whole copy");
            checked = true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let got = reader.join().unwrap();
    assert!(got == picture, "{} of {} bytes, not the same (cut off?)", got.len(), picture.len());
    assert!(checked, "the reader never got 2 MB in");
}

/// X65: a Wayland app that stops reading a large X11 copy halfway: the
/// X11 app's transfer still runs to its end (it would wait forever), and
/// the next paste works.
#[test]
fn a_paste_given_up_halfway_does_not_stall_the_owner() {
    let Some(session) = xwm::session() else { return };
    let picture = data(1024 * 1024, 3);
    let finished = x11_owner(&session, vec![("image/png", picture.clone()), ("UTF8_STRING", b"after".to_vec())], false);
    assert!(session.wait_for_log("selection set by an X11 app", Duration::from_secs(10)), "the compositor never saw the X11 copy");
    let mut app = wayland_app(&session);
    app.until("no clipboard offered", |s| s.selection.is_some());
    let (offer, _) = app.state.selection.clone().unwrap();
    let (read, write) = std::io::pipe().unwrap();
    offer.receive("image/png".to_owned(), write.as_fd());
    drop(write);
    app.roundtrip();
    let mut some = vec![0u8; 10 * 1024];
    std::io::Read::read_exact(&mut { &read }, &mut some).unwrap();
    drop(read);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        app.roundtrip();
        if finished.try_recv().is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "the X11 app's transfer stalled after the reader gave up");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(app.paste("text/plain;charset=utf-8"), b"after");
}

/// X66: two reads of an X11 copy at once, of different types, both
/// arrive right.
#[test]
fn two_pastes_at_once() {
    let Some(session) = xwm::session() else { return };
    let (a, b) = (data(300 * 1024, 4), data(200 * 1024, 5));
    x11_owner(&session, vec![("image/png", a.clone()), ("text/html", b.clone())], false);
    assert!(session.wait_for_log("selection set by an X11 app", Duration::from_secs(10)), "the compositor never saw the X11 copy");
    let mut app = wayland_app(&session);
    app.until("no clipboard offered", |s| s.selection.is_some());
    let (offer, _) = app.state.selection.clone().unwrap();
    let reads: Vec<_> = ["image/png", "text/html"]
        .into_iter()
        .map(|mime| {
            let (read, write) = std::io::pipe().unwrap();
            offer.receive(mime.to_owned(), write.as_fd());
            drop(write);
            std::thread::spawn(move || {
                let mut got = Vec::new();
                std::io::Read::read_to_end(&mut { read }, &mut got).map(|_| got)
            })
        })
        .collect();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !reads.iter().all(|r| r.is_finished()) {
        assert!(Instant::now() < deadline, "the two pastes never ended");
        app.roundtrip();
        std::thread::sleep(Duration::from_millis(10));
    }
    let got: Vec<Vec<u8>> = reads.into_iter().map(|r| r.join().unwrap().unwrap()).collect();
    assert!(got[0] == a, "the image: {} of {} bytes", got[0].len(), a.len());
    assert!(got[1] == b, "the HTML: {} of {} bytes", got[1].len(), b.len());
}

/// X67: an X11 app that asks twice and reads neither answer (fcitx, then
/// Slack): the clipboard is not held up for other apps.
#[test]
fn an_app_abandoning_its_requests_does_not_hold_up_others() {
    let Some(session) = xwm::session() else { return };
    let mut app = wayland_app(&session);
    app.copy("text/plain;charset=utf-8", &data(4 * 1024, 6));
    let got = while_serving(&mut app, || {
        let rude = Reader::new(&session);
        rude.ask("UTF8_STRING", "FIRST");
        rude.ask("UTF8_STRING", "SECOND");
        std::thread::sleep(Duration::from_millis(500));
        let polite = Reader::new(&session);
        let got = polite.read("UTF8_STRING");
        drop(rude);
        got
    });
    assert_eq!(got.map(|g| g.len()), Some(4 * 1024), "another app's paste");
}

/// X68: an X11 app whose window goes away in the middle of a large paste:
/// the transfer is dropped, and the next copy and paste work.
#[test]
fn a_requestor_gone_halfway_does_not_hold_up_others() {
    let Some(session) = xwm::session() else { return };
    let mut app = wayland_app(&session);
    let big = data(2 * 1024 * 1024, 7);
    app.copy("image/png", &big);
    while_serving(&mut app, || {
        let leaving = Reader::new(&session);
        leaving.ask("image/png", "HELIX_TEST");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Event::SelectionNotify(_) = leaving.next_event(deadline, "no answer to the first request") {
                break;
            }
        }
        leaving.conn.destroy_window(leaving.window).unwrap();
        leaving.conn.flush().unwrap();
        drop(leaving);
        std::thread::sleep(Duration::from_millis(300));
    });
    app.copy("text/plain;charset=utf-8", b"next copy");
    let got = while_serving(&mut app, || Reader::new(&session).read("UTF8_STRING"));
    assert_eq!(got.as_deref(), Some(&b"next copy"[..]), "the next paste");
}

/// X69: asking for a type the copy does not have is refused at once,
/// every time, and the clipboard still works after.
#[test]
fn unknown_types_are_refused_at_once() {
    let Some(session) = xwm::session() else { return };
    let mut app = wayland_app(&session);
    app.copy("text/plain;charset=utf-8", b"real");
    while_serving(&mut app, || {
        let reader = Reader::new(&session);
        let start = Instant::now();
        for _ in 0..100 {
            assert_eq!(reader.read("application/x-nothing-like-it"), None, "an unknown type was not refused");
        }
        assert!(start.elapsed() < Duration::from_secs(10), "100 refusals took {:?}", start.elapsed());
        assert_eq!(reader.read("UTF8_STRING").as_deref(), Some(&b"real"[..]));
    });
}

/// X70: plain text from a Wayland app is offered under every X11 text
/// target (`xclip` asks for STRING).
#[test]
fn wayland_text_under_every_x11_text_target() {
    let Some(session) = xwm::session() else { return };
    let mut app = wayland_app(&session);
    app.copy("text/plain;charset=utf-8", b"plain text");
    let got = while_serving(&mut app, || {
        let reader = Reader::new(&session);
        ["UTF8_STRING", "STRING", "TEXT"].map(|t| reader.read(t))
    });
    let wrong: Vec<String> = ["UTF8_STRING", "STRING", "TEXT"].iter().zip(got).filter(|(_, g)| g.as_deref() != Some(&b"plain text"[..])).map(|(t, g)| format!("{t}: {g:?}")).collect();
    assert!(wrong.is_empty(), "{wrong:?}");
}

/// X71: an X11 app that takes the clipboard and never answers (hung):
/// once another app copies, pasting works again.
#[test]
fn a_hung_owner_does_not_break_the_clipboard_for_good() {
    let Some(session) = xwm::session() else { return };
    let mut app = wayland_app(&session);
    // It never even says what it offers (TARGETS).
    x11_owner(&session, vec![("UTF8_STRING", b"never".to_vec())], true);
    std::thread::sleep(Duration::from_millis(500));
    app.roundtrip();
    x11_owner(&session, vec![("UTF8_STRING", b"working".to_vec())], false);
    app.until("the next X11 copy was never offered", |s| s.selection.is_some());
    assert_eq!(app.paste("text/plain;charset=utf-8"), b"working");
}

/// X96 (W65): an X11 app taking CLIPBOARD and PRIMARY at once, with
/// different types: each reaches Wayland apps with its own types (the
/// answers about the two didn't cross).
#[test]
fn clipboard_and_primary_taken_at_once_keep_their_own_types() {
    let Some(session) = xwm::session() else { return };
    let mut app = wayland_app(&session);
    assert!(app.primary_device.is_some(), "helix-comp offers no primary selection");
    let _owner = x11_owner_of(
        &session,
        vec![
            ("CLIPBOARD", vec![("text/html", b"<i>clip</i>".to_vec()), ("image/png", data(100, 2)), ("application/x-helix-clip", b"clip".to_vec())]),
            ("PRIMARY", vec![("text/html", b"<i>primary</i>".to_vec()), ("image/png", data(100, 3))]),
        ],
        false,
    );
    let sorted = |types: &[String]| {
        let mut t: Vec<String> = types.iter().filter(|t| !t.starts_with("TARGETS")).cloned().collect();
        t.sort();
        t
    };
    app.until("the X11 clipboard and primary selection never both reached the Wayland app", |s| {
        let clip = s.selection.as_ref().is_some_and(|(_, t)| sorted(t).len() == 3);
        let primary = s.primary.as_ref().is_some_and(|(_, t)| !t.is_empty());
        clip && primary
    });
    let clip = sorted(&app.state.selection.as_ref().unwrap().1);
    let primary = sorted(&app.state.primary.as_ref().unwrap().1);
    assert_eq!(clip, ["application/x-helix-clip", "image/png", "text/html"], "the clipboard's types");
    assert_eq!(primary, ["image/png", "text/html"], "the primary selection's types");
}

/// X97 (W68): ownership flipping fast between an X11 app and a Wayland
/// app (Emacs and the window manager) settles on the last copy, which
/// pastes both ways (the window manager didn't end up asking itself).
#[test]
fn clipboard_ownership_flipping_fast_settles() {
    let Some(session) = xwm::session() else { return };
    let mut app = wayland_app(&session);
    let mut owners = Vec::new();
    for i in 0..10 {
        owners.push(x11_owner(&session, vec![("UTF8_STRING", format!("x11 {i}").into_bytes())], false));
        app.copy("text/plain;charset=utf-8", format!("wayland {i}").as_bytes());
    }
    let until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < until {
        app.roundtrip();
        std::thread::sleep(Duration::from_millis(10));
    }
    // The Wayland app's copy was last: it pastes it itself, and so does
    // an X11 app.
    assert_eq!(String::from_utf8_lossy(&app.paste("text/plain;charset=utf-8")), "wayland 9", "the Wayland app's own paste");
    let got = while_serving(&mut app, || {
        let reader = Reader::new(&session);
        reader.read("UTF8_STRING")
    });
    let shown = got.as_ref().map(|g| String::from_utf8_lossy(g).into_owned());
    assert_eq!(shown.as_deref(), Some("wayland 9"), "the X11 app's paste");
}

/// After such a flip, the next X11 copy still reaches Wayland apps (the
/// clipboard isn't broken for good).
#[test]
fn the_clipboard_recovers_after_ownership_flipped_fast() {
    let Some(session) = xwm::session() else { return };
    let mut app = wayland_app(&session);
    let _first = x11_owner(&session, vec![("UTF8_STRING", b"x11 first".to_vec())], false);
    app.copy("text/plain;charset=utf-8", b"wayland");
    std::thread::sleep(Duration::from_millis(300));
    let _last = x11_owner(&session, vec![("UTF8_STRING", b"x11 last".to_vec())], false);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        app.roundtrip();
        let ready = app.state.selection.as_ref().is_some_and(|(_, t)| t.iter().any(|t| t == "text/plain;charset=utf-8"));
        if ready && app.paste("text/plain;charset=utf-8") == b"x11 last" {
            break;
        }
        assert!(Instant::now() < deadline, "the next X11 copy never reached the Wayland app");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// An X11 owner with a quirk (#234): it refuses TARGETS (old Xt and Motif
/// apps), or answers it with another time than the request's. It owns
/// CLIPBOARD with `text` as UTF8_STRING.
fn quirky_owner(session: &HeadlessSession, refuse_targets: bool, time_offset: u32, text: &'static [u8]) {
    let (conn, screen_num) = common::x11(session);
    let root = conn.setup().roots[screen_num].root;
    let window = helper_window(&conn, root, EventMask::NO_EVENT);
    let (clipboard, targets, utf8) = (atom(&conn, "CLIPBOARD"), atom(&conn, "TARGETS"), atom(&conn, "UTF8_STRING"));
    conn.set_selection_owner(window, clipboard, x11rb::CURRENT_TIME).unwrap();
    conn.flush().unwrap();
    std::thread::spawn(move || {
        while let Ok(event) = conn.wait_for_event() {
            let Event::SelectionRequest(request) = event else { continue };
            let property = if request.target == targets && !refuse_targets {
                conn.change_property32(PropMode::REPLACE, request.requestor, request.property, AtomEnum::ATOM, &[targets, utf8]).unwrap();
                request.property
            } else if request.target == utf8 {
                conn.change_property8(PropMode::REPLACE, request.requestor, request.property, utf8, text).unwrap();
                request.property
            } else {
                x11rb::NONE
            };
            let time = if request.target == targets { request.time.wrapping_add(time_offset) } else { request.time };
            let notify = SelectionNotifyEvent { response_type: SELECTION_NOTIFY_EVENT, sequence: 0, time, requestor: request.requestor, selection: request.selection, target: request.target, property };
            conn.send_event(false, request.requestor, EventMask::NO_EVENT, notify).unwrap();
            conn.flush().unwrap();
        }
    });
}

/// #234: an X11 owner that refuses TARGETS (old Xt and Motif apps) is
/// offered to Wayland apps as text, which pastes; not left unoffered with
/// the last copy's offer still up.
#[test]
fn an_owner_refusing_targets_is_pasted_as_text() {
    if common::known_failure(&[common::Xwm::Smithay], "an X11 owner refusing TARGETS is never offered to Wayland apps (#234)") {
        return;
    }
    let Some(session) = xwm::session() else { return };
    let mut app = wayland_app(&session);
    app.copy("text/plain;charset=utf-8", b"the Wayland copy");
    app.roundtrip();
    quirky_owner(&session, true, 0, b"from an old X11 app");
    app.until("the X11 copy wasn't offered", |s| s.selection.as_ref().is_some_and(|(_, types)| types.iter().any(|t| t == "text/plain;charset=utf-8")) && s.selection.as_ref().is_some_and(|(_, t)| !t.is_empty()));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if app.paste("text/plain;charset=utf-8") == b"from an old X11 app" {
            break;
        }
        assert!(Instant::now() < deadline, "the X11 copy of an owner refusing TARGETS doesn't paste");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// #234: an owner answering TARGETS with another time than the request's
/// is still the owner asked: its types are offered.
#[test]
fn targets_answered_with_another_time_are_taken() {
    let Some(session) = xwm::session() else { return };
    let mut app = wayland_app(&session);
    quirky_owner(&session, false, 1, b"answered late");
    app.until("the X11 copy wasn't offered", |s| s.selection.as_ref().is_some_and(|(_, types)| types.iter().any(|t| t == "text/plain;charset=utf-8")));
    assert_eq!(app.paste("text/plain;charset=utf-8"), b"answered late");
}

/// #234: one X11 window asking for two types at once, into two
/// properties, gets both (the "older requests end" rule is for the same
/// property).
#[test]
fn two_types_asked_for_at_once_by_one_window() {
    let Some(session) = xwm::session() else { return };
    let mut app = wayland_app(&session);
    app.copy("text/plain;charset=utf-8", b"twice");
    let got = while_serving(&mut app, || {
        let reader = Reader::new(&session);
        reader.ask("UTF8_STRING", "HELIX_A");
        reader.ask("STRING", "HELIX_B");
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut answered = Vec::new();
        while answered.len() < 2 {
            if let Event::SelectionNotify(n) = reader.next_event(deadline, "not both requests were answered") {
                answered.push(n.property);
            }
        }
        answered.iter().map(|p| if *p == x11rb::NONE { None } else { Some(reader.conn.get_property(true, reader.window, *p, AtomEnum::ANY, 0, 1024).unwrap().reply().unwrap().value) }).collect::<Vec<_>>()
    });
    assert_eq!(got, vec![Some(b"twice".to_vec()), Some(b"twice".to_vec())], "one of the two requests was refused");
}

const MULTIPLE_WHY: &str = "Smithay's window manager refuses MULTIPLE requests (#283)";

/// X133: MULTIPLE (ICCCM 2.6.2: several targets in one request, as
/// target and property pairs in an ATOM_PAIR property): each pair is
/// converted as a request of its own (text, an image, TARGETS listing
/// MULTIPLE, TIMESTAMP), a pair asking for a type the copy doesn't have
/// or for MULTIPLE again gets None for its property, one answer comes
/// for the whole, and the clipboard goes on working.
#[test]
fn a_multiple_request_is_answered_at_once() {
    let Some(session) = xwm::session() else { return };
    if common::known_failure(&[common::Xwm::Smithay], MULTIPLE_WHY) {
        return;
    }
    let mut app = wayland_app(&session);
    let picture = data(5000, 3);
    app.copy_types(&[("text/plain;charset=utf-8", b"several"), ("image/png", &picture)]);
    let (pairs, values, targets, stray, after) = while_serving(&mut app, || {
        let reader = Reader::new(&session);
        let deadline = Instant::now() + Duration::from_secs(10);
        let asked = [("UTF8_STRING", "HELIX_A"), ("image/png", "HELIX_B"), ("HELIX_BOGUS", "HELIX_C"), ("MULTIPLE", "HELIX_D"), ("TIMESTAMP", "HELIX_E"), ("TARGETS", "HELIX_F")];
        let pairs = reader.ask_multiple(&asked, deadline).expect("a MULTIPLE request was refused");
        let value = |name: &str| reader.conn.get_property(true, reader.window, atom(&reader.conn, name), AtomEnum::ANY, 0, u32::MAX / 4).unwrap().reply().unwrap();
        let values: Vec<(Vec<u8>, u32)> = ["HELIX_A", "HELIX_B", "HELIX_C", "HELIX_D", "HELIX_E"].iter().map(|n| { let r = value(n); (r.value, r.format.into()) }).collect();
        let targets: Vec<u32> = value("HELIX_F").value32().map(Iterator::collect).unwrap_or_default();
        let names: Vec<String> = targets.iter().map(|a| String::from_utf8_lossy(&reader.conn.get_atom_name(*a).unwrap().reply().unwrap().name).into_owned()).collect();
        // One answer for the whole.
        let end = Instant::now() + Duration::from_millis(500);
        let mut stray = 0;
        while Instant::now() < end {
            match reader.conn.poll_for_event().unwrap() {
                Some(Event::SelectionNotify(_)) => stray += 1,
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        (pairs, values, names, stray, reader.read("UTF8_STRING"))
    });
    let ok: Vec<bool> = pairs.iter().map(Option::is_some).collect();
    assert_eq!(ok, [true, true, false, false, true, true], "which pairs were converted");
    assert_eq!(values[0].0, b"several", "the text pair");
    assert!(values[1].0 == picture, "the image pair: {} of {} bytes", values[1].0.len(), picture.len());
    assert!(values[2].0.is_empty() && values[3].0.is_empty(), "a failed pair's property was written");
    assert_eq!((values[4].0.len(), values[4].1), (4, 32), "the TIMESTAMP pair");
    for name in ["TARGETS", "TIMESTAMP", "MULTIPLE", "UTF8_STRING", "image/png"] {
        assert!(targets.iter().any(|t| t == name), "TARGETS in a MULTIPLE request lacks {name}: {targets:?}");
    }
    assert_eq!(stray, 0, "more than one answer to a MULTIPLE request");
    assert_eq!(after.as_deref(), Some(&b"several"[..]), "the clipboard stopped working after a MULTIPLE request");
}

/// A Wayland copy's TARGETS list MULTIPLE (ICCCM 2.6.2: owners support it).
#[test]
fn targets_list_multiple() {
    let Some(session) = xwm::session() else { return };
    if common::known_failure(&[common::Xwm::Smithay], MULTIPLE_WHY) {
        return;
    }
    let mut app = wayland_app(&session);
    app.copy("text/plain;charset=utf-8", b"listed");
    let names = while_serving(&mut app, || {
        let reader = Reader::new(&session);
        let list = reader.read("TARGETS").expect("TARGETS refused");
        list.as_chunks::<4>().0.iter().map(|a| u32::from_ne_bytes(*a)).map(|a| String::from_utf8_lossy(&reader.conn.get_atom_name(a).unwrap().reply().unwrap().name).into_owned()).collect::<Vec<_>>()
    });
    assert!(names.iter().any(|n| n == "MULTIPLE"), "TARGETS lacks MULTIPLE: {names:?}");
}

/// A pair of a MULTIPLE request too large for one property goes
/// incrementally (ICCCM 2.7.2, per pair) once the whole is answered,
/// beside a small one.
#[test]
fn a_large_pair_of_a_multiple_request_goes_incrementally() {
    let Some(session) = xwm::session() else { return };
    if common::known_failure(&[common::Xwm::Smithay], MULTIPLE_WHY) {
        return;
    }
    let mut app = wayland_app(&session);
    let picture = data(1024 * 1024 + 77, 5);
    app.copy_types(&[("text/plain;charset=utf-8", b"small"), ("image/png", &picture)]);
    let (pairs, image, text) = while_serving(&mut app, || {
        let reader = Reader::new(&session);
        let deadline = Instant::now() + Duration::from_secs(20);
        let pairs = reader.ask_multiple(&[("image/png", "HELIX_A"), ("UTF8_STRING", "HELIX_B")], deadline).expect("a MULTIPLE request was refused");
        let image = reader.take(atom(&reader.conn, "HELIX_A"), deadline);
        let text = reader.take(atom(&reader.conn, "HELIX_B"), deadline);
        (pairs, image, text)
    });
    assert!(pairs.iter().all(Option::is_some), "a pair failed: {pairs:?}");
    assert!(image.0 == picture, "{} of {} bytes, not the same", image.0.len(), picture.len());
    assert!(image.1 > 1, "a 1 MiB pair did not go incrementally");
    assert_eq!(text, (b"small".to_vec(), 1));
}

/// A MULTIPLE request without a list of pairs is refused: none in the
/// property, a list of another format or type, an odd one, an empty one,
/// or no property at all; a good one after them is converted.
#[test]
fn a_malformed_multiple_request_is_refused() {
    let Some(session) = xwm::session() else { return };
    if common::known_failure(&[common::Xwm::Smithay], MULTIPLE_WHY) {
        return;
    }
    let mut app = wayland_app(&session);
    app.copy("text/plain;charset=utf-8", b"well formed");
    let (refused, good) = while_serving(&mut app, || {
        let reader = Reader::new(&session);
        let deadline = Instant::now() + Duration::from_secs(10);
        let (utf8, prop, pair) = (atom(&reader.conn, "UTF8_STRING"), atom(&reader.conn, "HELIX_A"), atom(&reader.conn, "ATOM_PAIR"));
        let mut refused = vec![
            ("8-bit", reader.ask_multiple_raw(pair, 8, &[utf8, prop], deadline).is_none()),
            ("STRING", reader.ask_multiple_raw(AtomEnum::STRING.into(), 32, &[utf8, prop], deadline).is_none()),
            ("odd", reader.ask_multiple_raw(pair, 32, &[utf8, prop, utf8], deadline).is_none()),
            ("empty", reader.ask_multiple_raw(pair, 32, &[], deadline).is_none()),
        ];
        // No property on the window.
        let none = atom(&reader.conn, "HELIX_NOTHING");
        reader.conn.delete_property(reader.window, none).unwrap();
        reader.conn.convert_selection(reader.window, reader.clipboard, atom(&reader.conn, "MULTIPLE"), none, x11rb::CURRENT_TIME).unwrap();
        reader.conn.flush().unwrap();
        refused.push(("missing", reader.answer(deadline).is_none()));
        // No property asked for (ICCCM: MULTIPLE needs one).
        reader.conn.convert_selection(reader.window, reader.clipboard, atom(&reader.conn, "MULTIPLE"), x11rb::NONE, x11rb::CURRENT_TIME).unwrap();
        reader.conn.flush().unwrap();
        refused.push(("no property", reader.answer(deadline).is_none()));
        let good = reader.ask_multiple(&[("UTF8_STRING", "HELIX_A")], deadline).map(|_| reader.take(prop, deadline).0);
        (refused, good)
    });
    for (what, was) in refused {
        assert!(was, "a MULTIPLE request ({what}) was not refused");
    }
    assert_eq!(good.as_deref(), Some(&b"well formed"[..]), "a good MULTIPLE request after malformed ones");
}

/// The requestor of a MULTIPLE request going away halfway through an
/// incremental pair: the transfer ends, and others are answered.
#[test]
fn a_multiple_requestor_gone_halfway_does_not_hold_up_others() {
    let Some(session) = xwm::session() else { return };
    if common::known_failure(&[common::Xwm::Smithay], MULTIPLE_WHY) {
        return;
    }
    let mut app = wayland_app(&session);
    let picture = data(2 * 1024 * 1024, 9);
    app.copy_types(&[("text/plain;charset=utf-8", b"still here"), ("image/png", &picture)]);
    let (pairs, after) = while_serving(&mut app, || {
        let pairs = {
            let reader = Reader::new(&session);
            let deadline = Instant::now() + Duration::from_secs(10);
            let pairs = reader.ask_multiple(&[("image/png", "HELIX_A"), ("UTF8_STRING", "HELIX_B")], deadline);
            // Start the incremental pair, take one chunk, and go.
            reader.conn.delete_property(reader.window, atom(&reader.conn, "HELIX_A")).unwrap();
            reader.conn.flush().unwrap();
            loop {
                if let Event::PropertyNotify(n) = reader.next_event(deadline, "no chunk came") {
                    if n.state == Property::NEW_VALUE && n.atom == atom(&reader.conn, "HELIX_A") {
                        break;
                    }
                }
            }
            pairs
        };
        let reader = Reader::new(&session);
        let deadline = Instant::now() + Duration::from_secs(10);
        let after = reader.ask_multiple(&[("UTF8_STRING", "HELIX_B")], deadline).map(|_| reader.take(atom(&reader.conn, "HELIX_B"), deadline).0);
        (pairs, after)
    });
    assert!(pairs.as_ref().is_some_and(|p| p.iter().all(Option::is_some)), "a MULTIPLE request was not converted: {pairs:?}");
    assert_eq!(after.as_deref(), Some(&b"still here"[..]), "a MULTIPLE request after its requestor went away halfway");
}

/// Xwayland going away (a crash) while a pair of a MULTIPLE request is
/// under way: it ends with Xwayland, and MULTIPLE works once it is back.
#[test]
fn xwayland_going_away_mid_multiple_request() {
    let Some(session) = xwm::session() else { return };
    if common::known_failure(&[common::Xwm::Smithay], MULTIPLE_WHY) {
        return;
    }
    let mut app = wayland_app(&session);
    let picture = data(2 * 1024 * 1024, 11);
    app.copy_types(&[("text/plain;charset=utf-8", b"after the restart"), ("image/png", &picture)]);
    let pairs = while_serving(&mut app, || {
        let reader = Reader::new(&session);
        let deadline = Instant::now() + Duration::from_secs(10);
        let pairs = reader.ask_multiple(&[("image/png", "HELIX_A")], deadline);
        // Kept open (not going away itself) until Xwayland does.
        std::mem::forget(reader);
        pairs
    });
    assert!(pairs.is_some_and(|p| p[0].is_some()), "a MULTIPLE request was not converted");
    let killed = xwm::kill_xwayland(&session);
    drop(xwm::restarted_xwayland(&session, killed));
    let after = while_serving(&mut app, || {
        let reader = Reader::new(&session);
        let deadline = Instant::now() + Duration::from_secs(10);
        reader.ask_multiple(&[("UTF8_STRING", "HELIX_B")], deadline).map(|_| reader.take(atom(&reader.conn, "HELIX_B"), deadline).0)
    });
    assert_eq!(after.as_deref(), Some(&b"after the restart"[..]), "a MULTIPLE request after Xwayland came back");
}

/// An X11 app owning the clipboard that answers TARGETS (`text`) and then
/// never sends the data; each data request is reported.
fn x11_owner_that_hangs(session: &HeadlessSession) -> mpsc::Receiver<()> {
    let (conn, screen_num) = common::x11(session);
    let root = conn.setup().roots[screen_num].root;
    let window = helper_window(&conn, root, EventMask::NO_EVENT);
    let (clipboard, targets, utf8) = (atom(&conn, "CLIPBOARD"), atom(&conn, "TARGETS"), atom(&conn, "UTF8_STRING"));
    conn.set_selection_owner(window, clipboard, x11rb::CURRENT_TIME).unwrap();
    conn.flush().unwrap();
    let (asked, asks) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(event) = conn.wait_for_event() {
            let Event::SelectionRequest(request) = event else { continue };
            if request.target != targets {
                let _ = asked.send(());
                continue;
            }
            conn.change_property32(PropMode::REPLACE, request.requestor, request.property, AtomEnum::ATOM, &[targets, utf8]).unwrap();
            let notify = SelectionNotifyEvent { response_type: SELECTION_NOTIFY_EVENT, sequence: 0, time: request.time, requestor: request.requestor, selection: request.selection, target: request.target, property: request.property };
            conn.send_event(false, request.requestor, EventMask::NO_EVENT, notify).unwrap();
            conn.flush().unwrap();
        }
    });
    asks
}

/// Xwayland going away (a crash) in the middle of a paste from an X11
/// app: the Wayland app's paste ends instead of waiting for good, the X11
/// copy is no longer offered, and a Wayland copy pastes into X11 apps
/// once Xwayland is back.
#[test]
fn xwayland_going_away_mid_paste_ends_the_paste() {
    let Some(session) = xwm::session() else { return };
    let mut app = wayland_app(&session);
    let asks = x11_owner_that_hangs(&session);
    app.until("the X11 copy wasn't offered", |s| s.selection.as_ref().is_some_and(|(_, types)| types.iter().any(|t| t == "text/plain;charset=utf-8")));
    let (offer, _) = app.state.selection.clone().unwrap();
    let (read, write) = std::io::pipe().unwrap();
    offer.receive("text/plain;charset=utf-8".into(), write.as_fd());
    drop(write);
    app.roundtrip();
    let reading = std::thread::spawn(move || std::io::Read::read_to_end(&mut { read }, &mut Vec::new()));
    let deadline = Instant::now() + Duration::from_secs(5);
    while asks.try_recv().is_err() {
        assert!(Instant::now() < deadline, "the X11 owner was never asked for the data");
        app.roundtrip();
        std::thread::sleep(Duration::from_millis(10));
    }
    let killed = xwm::kill_xwayland(&session);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !reading.is_finished() {
        assert!(Instant::now() < deadline, "a paste from an X11 app never ended after Xwayland went away");
        app.roundtrip();
        std::thread::sleep(Duration::from_millis(10));
    }
    app.until("the X11 copy is still offered after Xwayland went away", |s| s.selection.is_none());
    drop(xwm::restarted_xwayland(&session, killed));
    app.copy("text/plain;charset=utf-8", b"after the restart");
    let got = while_serving(&mut app, || Reader::new(&session).read("UTF8_STRING"));
    assert_eq!(got.as_deref(), Some(&b"after the restart"[..]), "a Wayland copy doesn't paste into X11 apps after Xwayland came back");
}

/// A Wayland copy made before Xwayland went away (a crash) pastes into
/// X11 apps once it is back, as it did before.
#[test]
fn a_wayland_copy_pastes_into_x11_after_xwayland_comes_back() {
    let Some(session) = xwm::session() else { return };
    let mut app = wayland_app(&session);
    app.copy("text/plain;charset=utf-8", b"before the restart");
    let before = while_serving(&mut app, || Reader::new(&session).read("UTF8_STRING"));
    assert_eq!(before.as_deref(), Some(&b"before the restart"[..]));
    let killed = xwm::kill_xwayland(&session);
    drop(xwm::restarted_xwayland(&session, killed));
    let got = while_serving(&mut app, || Reader::new(&session).read("UTF8_STRING"));
    assert_eq!(got.as_deref(), Some(&b"before the restart"[..]), "a Wayland copy made before Xwayland went away doesn't paste into X11 apps once it is back");
}
