//! XDND (X11 drag and drop) for headless tests: an X11 app's window that
//! can be a drag source or a drop target, written against the XDND spec
//! with x11rb, the way toolkits behave.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use helix_test_support::headless::HeadlessSession;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt, CreateWindowAux, EventMask, PropMode, SelectionNotifyEvent, Window, WindowClass, SELECTION_NOTIFY_EVENT};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

pub const BTN_LEFT: u32 = 0x110;
pub const XDND_VERSION: u32 = 5;
pub const DATA: &[u8] = b"dragged words";

pub fn session() -> Option<HeadlessSession> {
    let comp = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-comp"));
    let options = helix_test_support::headless::Options { comp: &comp, shell: None, size: (800, 600), settings: "", env: Vec::new() };
    let session = HeadlessSession::start(options)?;
    if super::no_xwayland(&session) {
        return None;
    }
    Some(session)
}

#[derive(Clone, Copy)]
pub struct Atoms {
    pub aware: u32,
    pub proxy: u32,
    pub selection: u32,
    pub enter: u32,
    pub position: u32,
    pub status: u32,
    pub leave: u32,
    pub drop: u32,
    pub finished: u32,
    pub copy: u32,
    pub utf8: u32,
    pub wm_state: u32,
    pub wm_name: u32,
}

/// An X11 client with one top-level window.
pub struct X11 {
    pub conn: Arc<RustConnection>,
    pub root: Window,
    pub window: Window,
    pub atoms: Atoms,
}

impl X11 {
    /// A window of `size`, mapped and managed; XDND-aware when `aware`.
    pub fn new(session: &HeadlessSession, size: (u16, u16), aware: bool) -> Self {
        let x = Self::unmapped(session, size, aware);
        x.conn.map_window(x.window).unwrap();
        x.conn.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while x.wm_state() != Some(1) {
            assert!(Instant::now() < deadline, "the window manager never managed the window");
            std::thread::sleep(Duration::from_millis(20));
        }
        x
    }

    /// The same, never mapped (a proxy or helper window).
    pub fn unmapped(session: &HeadlessSession, (width, height): (u16, u16), aware: bool) -> Self {
        let (conn, screen_num) = super::x11(session);
        let conn = Arc::new(conn);
        let screen = &conn.setup().roots[screen_num];
        let (root, white) = (screen.root, screen.white_pixel);
        let atom = |name: &str| conn.intern_atom(false, name.as_bytes()).unwrap().reply().unwrap().atom;
        let atoms = Atoms {
            aware: atom("XdndAware"),
            proxy: atom("XdndProxy"),
            selection: atom("XdndSelection"),
            enter: atom("XdndEnter"),
            position: atom("XdndPosition"),
            status: atom("XdndStatus"),
            leave: atom("XdndLeave"),
            drop: atom("XdndDrop"),
            finished: atom("XdndFinished"),
            copy: atom("XdndActionCopy"),
            utf8: atom("UTF8_STRING"),
            wm_state: atom("WM_STATE"),
            wm_name: atom("WM_NAME"),
        };
        let window = conn.generate_id().unwrap();
        let events = EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION | EventMask::ENTER_WINDOW;
        let aux = CreateWindowAux::new().background_pixel(white).event_mask(events);
        conn.create_window(0, window, root, 10, 10, width, height, 0, WindowClass::INPUT_OUTPUT, 0, &aux).unwrap();
        if aware {
            conn.change_property32(PropMode::REPLACE, window, atoms.aware, AtomEnum::ATOM, &[XDND_VERSION]).unwrap();
        }
        conn.flush().unwrap();
        X11 { conn, root, window, atoms }
    }

    pub fn atom(&self, name: &str) -> u32 {
        self.conn.intern_atom(false, name.as_bytes()).unwrap().reply().unwrap().atom
    }

    pub fn atom_name(&self, atom: u32) -> String {
        String::from_utf8_lossy(&self.conn.get_atom_name(atom).unwrap().reply().unwrap().name).into_owned()
    }

    pub fn wm_state(&self) -> Option<u32> {
        let reply = self.conn.get_property(false, self.window, self.atoms.wm_state, AtomEnum::ANY, 0, 2).unwrap().reply().unwrap();
        reply.value32().and_then(|mut v| v.next())
    }

    /// The window's rectangle on the root window.
    pub fn rect(&self) -> (i32, i32, i32, i32) {
        let t = self.conn.translate_coordinates(self.window, self.root, 0, 0).unwrap().reply().unwrap();
        let g = self.conn.get_geometry(self.window).unwrap().reply().unwrap();
        (i32::from(t.dst_x), i32::from(t.dst_y), i32::from(g.width), i32::from(g.height))
    }

    /// One 32-bit property value of `window`.
    pub fn get32(&self, window: Window, property: u32) -> Option<u32> {
        let reply = self.conn.get_property(false, window, property, AtomEnum::ANY, 0, 1).ok()?.reply().ok()?;
        reply.value32().and_then(|mut v| v.next())
    }

    /// `window`'s WM_NAME (empty without one).
    pub fn name_of(&self, window: Window) -> Vec<u8> {
        self.conn.get_property(false, window, self.atoms.wm_name, AtomEnum::ANY, 0, 256).unwrap().reply().unwrap().value
    }
}

pub fn inside((x, y, w, h): (i32, i32, i32, i32), (px, py): (i32, i32)) -> bool {
    px >= x && py >= y && px < x + w && py < y + h
}

/// A point well inside `a` and outside `b`.
pub fn point_in_not(a: (i32, i32, i32, i32), b: Option<(i32, i32, i32, i32)>) -> (i32, i32) {
    point_in_none(a, b.as_slice())
}

/// A point well inside `a` and outside each of `others`.
pub fn point_in_none(a: (i32, i32, i32, i32), others: &[(i32, i32, i32, i32)]) -> (i32, i32) {
    let (x, y, w, h) = a;
    (x + 10..x + w - 10)
        .step_by(10)
        .flat_map(|px| (y + 10..y + h - 10).step_by(10).map(move |py| (px, py)))
        .find(|p| others.iter().all(|b| !inside(*b, *p)))
        .expect("no point of one window outside the others")
}

/// Send a ClientMessage of `kind` to `to`.
pub fn message(conn: &RustConnection, to: Window, kind: u32, data: [u32; 5]) {
    message_as(conn, to, to, kind, data);
}

/// The same, with the event's window field `window` (XDND proxies get
/// messages naming the window they stand in for).
pub fn message_as(conn: &RustConnection, to: Window, window: Window, kind: u32, data: [u32; 5]) {
    let event = ClientMessageEvent::new(32, window, kind, data);
    conn.send_event(false, to, EventMask::NO_EVENT, event).unwrap();
    conn.flush().unwrap();
}

/// The XDND-aware window under (`x`, `y`) of the root, looking down from
/// the top-level window there, as toolkits do (not `own`'s windows).
/// With `named_only`, windows without a WM_NAME are skipped, as Chromium
/// does.
fn find_target(x: &X11, (px, py): (i16, i16), own: &[Window], named_only: bool) -> Option<Window> {
    let mut window = x.root;
    loop {
        let child = x.conn.translate_coordinates(x.root, window, px, py).ok()?.reply().ok()?.child;
        if child == x11rb::NONE {
            return None;
        }
        if !own.contains(&child) && x.get32(child, x.atoms.aware).is_some() && (!named_only || !x.name_of(child).is_empty()) {
            return Some(child);
        }
        window = child;
    }
}

/// What the X11 drag source saw.
#[derive(Debug, PartialEq)]
pub enum SourceSaw {
    /// Positions went to this window.
    Target(Window),
    /// The target accepted (XdndStatus), in a message for this window.
    Accepted(Window),
    /// The data was asked for from this selection owner.
    Sent(Window),
    /// The target finished (XdndFinished), in a message for this window,
    /// with this data.
    Finished(Window, [u32; 5]),
}

/// How an X11 drag source behaves.
#[derive(Default, Clone, Copy)]
pub struct Source {
    /// Send the XDND messages from this window of the same app (its
    /// `window` field) instead of the selection owner, as Chromium does.
    pub messenger: Option<Window>,
    /// An override-redirect drag icon kept under the pointer.
    pub icon: Option<Window>,
    /// Only take windows with a WM_NAME as targets, as Chromium does.
    pub named_only: bool,
}

/// Run an XDND drag source for `x`'s window.
pub fn xdnd_source(x: X11) -> mpsc::Receiver<SourceSaw> {
    xdnd_source_with(x, Source::default())
}

/// Run an XDND drag source for `x`'s window: when the button is pressed
/// it owns XdndSelection, follows the pointer (its implicit grab) with
/// XdndPosition to the aware window under it, drops when the button is
/// released, and serves `DATA` as UTF8_STRING.
pub fn xdnd_source_with(x: X11, how: Source) -> mpsc::Receiver<SourceSaw> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let a = x.atoms;
        let from = how.messenger.unwrap_or(x.window);
        let own: Vec<Window> = [Some(x.window), how.messenger, how.icon].into_iter().flatten().collect();
        let mut target: Option<Window> = None;
        while let Ok(event) = x.conn.wait_for_event() {
            match event {
                Event::ButtonPress(press) => {
                    x.conn.set_selection_owner(x.window, a.selection, press.time).unwrap();
                    if let Some(icon) = how.icon {
                        let at = ConfigureWindowAux::new().x(i32::from(press.root_x) - 4).y(i32::from(press.root_y) - 4);
                        x.conn.configure_window(icon, &at).unwrap();
                        x.conn.map_window(icon).unwrap();
                    }
                    x.conn.flush().unwrap();
                }
                Event::MotionNotify(motion) => {
                    let time = motion.time;
                    if let Some(icon) = how.icon {
                        // Under the pointer, as drag icons are.
                        let at = ConfigureWindowAux::new().x(i32::from(motion.root_x) - 4).y(i32::from(motion.root_y) - 4);
                        x.conn.configure_window(icon, &at).unwrap();
                        x.conn.flush().unwrap();
                    }
                    let under = find_target(&x, (motion.root_x, motion.root_y), &own, how.named_only);
                    if under != target {
                        if let Some(old) = target {
                            message(&x.conn, old, a.leave, [from, 0, 0, 0, 0]);
                        }
                        if let Some(new) = under {
                            message(&x.conn, new, a.enter, [from, XDND_VERSION << 24, a.utf8, 0, 0]);
                            let _ = tx.send(SourceSaw::Target(new));
                        }
                        target = under;
                    }
                    if let Some(t) = target {
                        let at = (u32::from(motion.root_x as u16) << 16) | u32::from(motion.root_y as u16);
                        message(&x.conn, t, a.position, [from, 0, at, time, a.copy]);
                    }
                }
                Event::ButtonRelease(release) => {
                    if let Some(t) = target {
                        message(&x.conn, t, a.drop, [from, 0, release.time, 0, 0]);
                    }
                    if let Some(icon) = how.icon {
                        x.conn.unmap_window(icon).unwrap();
                        x.conn.flush().unwrap();
                    }
                }
                Event::ClientMessage(m) if m.type_ == a.status => {
                    if m.data.as_data32()[1] & 1 == 1 {
                        let _ = tx.send(SourceSaw::Accepted(m.window));
                    }
                }
                Event::ClientMessage(m) if m.type_ == a.finished => {
                    let _ = tx.send(SourceSaw::Finished(m.window, m.data.as_data32()));
                }
                Event::SelectionRequest(request) if request.selection == a.selection => {
                    let property = if request.target == a.utf8 {
                        x.conn.change_property8(PropMode::REPLACE, request.requestor, request.property, a.utf8, DATA).unwrap();
                        let _ = tx.send(SourceSaw::Sent(request.owner));
                        request.property
                    } else {
                        x11rb::NONE
                    };
                    let notify = SelectionNotifyEvent { response_type: SELECTION_NOTIFY_EVENT, sequence: 0, time: request.time, requestor: request.requestor, selection: request.selection, target: request.target, property };
                    x.conn.send_event(false, request.requestor, EventMask::NO_EVENT, notify).unwrap();
                    x.conn.flush().unwrap();
                }
                _ => {}
            }
        }
    });
    rx
}

/// What the X11 drop target saw.
#[derive(Debug, PartialEq)]
pub enum TargetSaw {
    /// XdndEnter, for this window (the event's window field), from this
    /// source window.
    Entered(Window, Window),
    Left,
    /// XdndDrop, from this source window, and the time it gave.
    Drop(Window, u32),
    /// It was dropped on, and fetched this (the first type).
    Dropped(Vec<u8>),
    /// It fetched each of these types, all asked for at once.
    DroppedAll(Vec<(u32, Vec<u8>)>),
}

/// What an X11 drop target does when dropped on.
#[derive(Clone, Default)]
pub struct Target {
    /// Wait this long before asking for the data.
    pub fetch_after: Duration,
    /// Ask for every offered type at once (from XdndEnter), not only the
    /// first.
    pub fetch_all: bool,
    /// With `fetch_all`, ask for one type after the other's answer
    /// instead.
    pub one_by_one: bool,
    /// Neither fetch nor finish: the test does (or doesn't).
    pub leave_to_test: bool,
}

/// Run an XDND drop target for `x`'s (XDND-aware) window.
pub fn xdnd_target_app(x: X11) -> mpsc::Receiver<TargetSaw> {
    xdnd_target_with(x, Target::default())
}

/// Run an XDND drop target for `x`'s (XDND-aware) window: it accepts the
/// first type offered, and fetches it when dropped on, as `how` says.
pub fn xdnd_target_with(x: X11, how: Target) -> mpsc::Receiver<TargetSaw> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let a = x.atoms;
        let mut source = x11rb::NONE;
        let mut target = x.window;
        let mut kinds = vec![a.utf8];
        let mut asked: Vec<(u32, u32)> = Vec::new();
        let mut queued: Vec<(u32, u32)> = Vec::new();
        let mut drop_time = 0;
        let mut got: Vec<(u32, Vec<u8>)> = Vec::new();
        let base = x.conn.intern_atom(false, b"HELIX_DROPPED").unwrap().reply().unwrap().atom;
        while let Ok(event) = x.conn.wait_for_event() {
            match event {
                Event::ClientMessage(m) if m.type_ == a.enter => {
                    let data = m.data.as_data32();
                    source = data[0];
                    target = m.window;
                    let offered: Vec<u32> = data[2..].iter().copied().filter(|k| *k != x11rb::NONE).collect();
                    if !offered.is_empty() {
                        kinds = offered;
                    }
                    let _ = tx.send(TargetSaw::Entered(m.window, source));
                }
                Event::ClientMessage(m) if m.type_ == a.position => {
                    message(&x.conn, source, a.status, [target, 1, 0, 0, a.copy]);
                }
                Event::ClientMessage(m) if m.type_ == a.leave => {
                    let _ = tx.send(TargetSaw::Left);
                }
                Event::ClientMessage(m) if m.type_ == a.drop => {
                    let time = m.data.as_data32()[2];
                    let _ = tx.send(TargetSaw::Drop(source, time));
                    if how.leave_to_test {
                        continue;
                    }
                    std::thread::sleep(how.fetch_after);
                    let wanted = if how.fetch_all { kinds.clone() } else { kinds[..1].to_vec() };
                    asked.clear();
                    got.clear();
                    drop_time = time;
                    queued = wanted.into_iter().enumerate().map(|(i, kind)| (kind, if i == 0 { base } else { x.atom(&format!("HELIX_DROPPED_{i}")) })).collect();
                    let now = if how.one_by_one { 1 } else { queued.len() };
                    for (kind, property) in queued.drain(..now) {
                        x.conn.convert_selection(x.window, a.selection, kind, property, time).unwrap();
                        asked.push((kind, property));
                    }
                    x.conn.flush().unwrap();
                }
                Event::SelectionNotify(n) if n.selection == a.selection => {
                    let Some(i) = asked.iter().position(|(k, _)| *k == n.target) else { continue };
                    let (kind, property) = asked.remove(i);
                    let data = if n.property == x11rb::NONE {
                        Vec::new()
                    } else {
                        x.conn.get_property(true, x.window, property, AtomEnum::ANY, 0, 1 << 20).unwrap().reply().unwrap().value
                    };
                    got.push((kind, data));
                    if !queued.is_empty() {
                        let (kind, property) = queued.remove(0);
                        x.conn.convert_selection(x.window, a.selection, kind, property, drop_time).unwrap();
                        x.conn.flush().unwrap();
                        asked.push((kind, property));
                    }
                    if !asked.is_empty() {
                        continue;
                    }
                    message(&x.conn, source, a.finished, [target, 1, a.copy, 0, 0]);
                    if how.fetch_all {
                        let _ = tx.send(TargetSaw::DroppedAll(std::mem::take(&mut got)));
                    } else {
                        let _ = tx.send(TargetSaw::Dropped(got.remove(0).1));
                    }
                }
                _ => {}
            }
        }
    });
    rx
}

/// Wait for `want` among what `rx` reports (others are skipped).
pub fn expect<T: std::fmt::Debug + PartialEq>(rx: &mpsc::Receiver<T>, want: impl Fn(&T) -> bool, what: &str) -> T {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(seen) if want(&seen) => return seen,
            Ok(_) => {}
            Err(_) => panic!("{what}"),
        }
    }
}

/// Move the pointer to `to` in steps (the drag follows each).
pub fn glide(session: &mut HeadlessSession, from: (i32, i32), to: (i32, i32)) {
    for i in 1..=8 {
        let x = from.0 + (to.0 - from.0) * i / 8;
        let y = from.1 + (to.1 - from.1) * i / 8;
        session.input(&format!("move {x} {y}"));
        std::thread::sleep(Duration::from_millis(30));
    }
}
