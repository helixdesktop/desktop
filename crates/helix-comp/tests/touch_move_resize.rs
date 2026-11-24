//! Wayland apps moving and resizing their own windows by touch
//! (helixdesktop/desktop#293; helixdesktop/spec
//! `contracts/wayland/README.md`, "Apps moving and resizing their
//! windows"): GTK and Qt on a touch screen send `xdg_toplevel.move` and
//! `xdg_toplevel.resize` with the `wl_touch.down` serial. As mutter and
//! wlroots, the window follows that touch point until it lifts; a resize
//! by the top or left edge keeps the opposite edges in place, as by the
//! pointer. The serial of a touch on another client, or of a touch that
//! has lifted, starts nothing.
//!
//! The windows draw a client-side shadow around their window geometry
//! and are touched (and clicked) in it, where GTK has its resize borders:
//! those reach the app.
//!
//! Before the fix only a pointer press's serial was accepted: the touch
//! requests did nothing.
//!
//! Runs the real `helix-comp` headless; needs `dbus-daemon` and Xwayland
//! (as every headless session), skipped otherwise.

use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use helix_test_support::headless::{HeadlessSession, Options};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_compositor, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface, wl_touch};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

/// The virtual monitor.
const SIZE: (i32, i32) = (1280, 800);
/// A new window's size (its window geometry).
const START: (i32, i32) = (400, 300);
/// The client-side shadow around it, as GTK draws.
const SHADOW: i32 = 24;

#[derive(Default)]
struct State {
    /// The surface under the pointer and where on it (surface-local).
    pointer: Option<(wl_surface::WlSurface, f64, f64)>,
    /// The serial of the last button press.
    press: Option<u32>,
    /// The last touch down: its serial, surface and where on it.
    down: Option<(u32, wl_surface::WlSurface, f64, f64)>,
    /// Touch motions and lifts this client was sent.
    motions: usize,
    ups: usize,
    /// The last toplevel configure: size and whether resizing.
    configure: Option<((i32, i32), bool)>,
    /// A configure to ack.
    pending: Option<u32>,
}

macro_rules! quiet {
    ($($iface:ty),*) => {$(
        impl Dispatch<$iface, ()> for State {
            fn event(_: &mut Self, _: &$iface, _: <$iface as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
quiet!(wl_compositor::WlCompositor, wl_surface::WlSurface, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer, wl_seat::WlSeat);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(_: &mut Self, base: &xdg_wm_base::XdgWmBase, event: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}
impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(state: &mut Self, _: &xdg_toplevel::XdgToplevel, event: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_toplevel::Event::Configure { width, height, states } = event {
            let resizing = xdg_toplevel::State::Resizing as u32;
            let r = states.as_chunks::<4>().0.iter().any(|c| u32::from_ne_bytes(*c) == resizing);
            state.configure = Some(((width, height), r));
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(state: &mut Self, _: &xdg_surface::XdgSurface, event: xdg_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = event {
            state.pending = Some(serial);
        }
    }
}
impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(state: &mut Self, _: &wl_pointer::WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_pointer::Event::Enter { surface, surface_x, surface_y, .. } => state.pointer = Some((surface, surface_x, surface_y)),
            wl_pointer::Event::Leave { .. } => state.pointer = None,
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                if let Some(p) = state.pointer.as_mut() {
                    (p.1, p.2) = (surface_x, surface_y);
                }
            }
            wl_pointer::Event::Button { serial, state: wayland_client::WEnum::Value(wl_pointer::ButtonState::Pressed), .. } => state.press = Some(serial),
            _ => {}
        }
    }
}
impl Dispatch<wl_touch::WlTouch, ()> for State {
    fn event(state: &mut Self, _: &wl_touch::WlTouch, event: wl_touch::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_touch::Event::Down { serial, surface, x, y, .. } => state.down = Some((serial, surface, x, y)),
            wl_touch::Event::Motion { .. } => state.motions += 1,
            wl_touch::Event::Up { .. } => state.ups += 1,
            _ => {}
        }
    }
}

struct Client {
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    compositor: wl_compositor::WlCompositor,
    shm: wl_shm::WlShm,
    base: xdg_wm_base::XdgWmBase,
    seat: wl_seat::WlSeat,
}

/// A toplevel that draws itself at whatever size it is configured to,
/// with [`SHADOW`] around its window geometry.
struct Toplevel {
    surface: wl_surface::WlSurface,
    xdg: xdg_surface::XdgSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    size: (i32, i32),
}

fn buffer(shm: &wl_shm::WlShm, qh: &QueueHandle<State>, (w, h): (i32, i32)) -> wl_buffer::WlBuffer {
    let bytes: Vec<u8> = std::iter::repeat_n(0xFF80_8080u32.to_le_bytes(), (w * h) as usize).flatten().collect();
    // SAFETY: memfd_create returns a new fd we own (or -1).
    let raw = unsafe { libc::memfd_create(c"touch-test".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(raw >= 0);
    // SAFETY: just created.
    let fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };
    std::io::Write::write_all(&mut std::fs::File::from(fd.try_clone().unwrap()), &bytes).unwrap();
    let pool = shm.create_pool(fd.as_fd(), bytes.len() as i32, qh, ());
    let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, ());
    pool.destroy();
    buffer
}

impl Client {
    fn connect(session: &HeadlessSession) -> Self {
        let conn = Connection::from_socket(UnixStream::connect(&session.wayland_display).unwrap()).unwrap();
        let (globals, queue) = registry_queue_init::<State>(&conn).unwrap();
        let qh = queue.handle();
        let seat: wl_seat::WlSeat = globals.bind(&qh, 5..=9, ()).unwrap();
        seat.get_pointer(&qh, ());
        seat.get_touch(&qh, ());
        Client {
            compositor: globals.bind(&qh, 4..=6, ()).unwrap(),
            shm: globals.bind(&qh, 1..=1, ()).unwrap(),
            base: globals.bind(&qh, 1..=6, ()).unwrap(),
            seat,
            queue,
            qh,
            state: State::default(),
        }
    }

    /// Handle events, answering a configure by drawing at its size.
    fn settle(&mut self, t: &mut Toplevel) {
        for _ in 0..3 {
            self.queue.roundtrip(&mut self.state).unwrap();
            let Some(serial) = self.state.pending.take() else { continue };
            t.xdg.ack_configure(serial);
            if let Some(((w, h), _)) = self.state.configure {
                if w > 0 && h > 0 {
                    t.size = (w, h);
                }
            }
            let full = (t.size.0 + 2 * SHADOW, t.size.1 + 2 * SHADOW);
            t.xdg.set_window_geometry(SHADOW, SHADOW, t.size.0, t.size.1);
            t.surface.attach(Some(&buffer(&self.shm, &self.qh, full)), 0, 0);
            t.surface.damage_buffer(0, 0, full.0, full.1);
            t.surface.commit();
        }
    }

    fn toplevel(&mut self) -> Toplevel {
        let surface = self.compositor.create_surface(&self.qh, ());
        let xdg = self.base.get_xdg_surface(&surface, &self.qh, ());
        let toplevel = xdg.get_toplevel(&self.qh, ());
        toplevel.set_title("touch test".into());
        surface.commit();
        let mut t = Toplevel { surface, xdg, toplevel, size: START };
        self.state.configure = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.state.configure.is_none() {
            assert!(Instant::now() < deadline, "no first configure");
            self.settle(&mut t);
        }
        self.settle(&mut t);
        t
    }

    /// Where `t`'s window geometry is on the screen, found with the
    /// pointer at `(x, y)` (which must be over it).
    fn origin_at(&mut self, session: &mut HeadlessSession, t: &mut Toplevel, (x, y): (i32, i32)) -> (i32, i32) {
        // A motion off and back makes sure the client hears where it is.
        session.input(&format!("move {} {y}", x + 1));
        session.input(&format!("move {x} {y}"));
        self.settle(t);
        match self.state.pointer.clone() {
            Some((surface, sx, sy)) if surface == t.surface => (x - (sx as i32 - SHADOW), y - (sy as i32 - SHADOW)),
            other => panic!("the pointer at {x},{y} is not on the window: {other:?}\n{}", tail(session)),
        }
    }

    /// Put a finger down at `(x, y)`: the serial of the down this client
    /// got there.
    fn touch_down(&mut self, session: &mut HeadlessSession, t: &mut Toplevel, (x, y): (i32, i32)) -> u32 {
        self.state.down = None;
        session.input(&format!("touch down {x} {y}"));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.settle(t);
            if let Some((serial, surface, ..)) = &self.state.down {
                assert!(surface == &t.surface, "the touch at {x},{y} went to another surface");
                return *serial;
            }
            assert!(Instant::now() < deadline, "the touch at {x},{y} never reached the window\n{}", tail(session));
        }
    }
}

fn session() -> Option<HeadlessSession> {
    let comp = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-comp"));
    HeadlessSession::start(Options {
        comp: &comp,
        shell: None,
        size: (SIZE.0 as u32, SIZE.1 as u32),
        // No edge tiling: nothing here goes near an edge, but be sure.
        settings: "[org/gnome/mutter]\nedge-tiling=false\n",
        env: vec![],
    })
}

fn tail(session: &HeadlessSession) -> String {
    let log = session.log();
    log.lines().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
}

/// Drag the finger that is down at `from` by `by` in ten steps (the
/// client drawing whatever it is configured to as it goes), then lift it.
fn drag(session: &mut HeadlessSession, c: &mut Client, t: &mut Toplevel, from: (i32, i32), by: (i32, i32)) {
    for step in 1..=10 {
        session.input(&format!("touch move {} {}", from.0 + step * by.0 / 10, from.1 + step * by.1 / 10));
        c.settle(t);
    }
    session.input("touch up");
    c.settle(t);
    c.settle(t);
}

/// A new window, and where its window geometry is on the screen.
fn window(session: &mut HeadlessSession, c: &mut Client) -> (Toplevel, (i32, i32)) {
    let mut t = c.toplevel();
    // New windows are centered on the (only) monitor.
    let origin = c.origin_at(session, &mut t, (SIZE.0 / 2, SIZE.1 / 2));
    (t, origin)
}

#[test]
fn an_app_moves_its_window_by_touch() {
    let Some(mut session) = session() else { return };
    let mut c = Client::connect(&session);
    let (mut t, origin) = window(&mut session, &mut c);
    // On its header bar.
    let at = (origin.0 + 150, origin.1 + 15);
    let serial = c.touch_down(&mut session, &mut t, at);
    t.toplevel._move(&c.seat, serial);
    c.settle(&mut t);
    let by = (150, 80);
    drag(&mut session, &mut c, &mut t, at, by);
    let moved = c.origin_at(&mut session, &mut t, (at.0 + by.0, at.1 + by.1 + 50));
    assert_eq!(moved, (origin.0 + by.0, origin.1 + by.1), "the window followed the finger (from {origin:?})\n{}", tail(&session));
    assert_eq!(c.state.ups, 1, "the app was told the finger lifted");

    // Once lifted, the finger no longer drags it.
    c.touch_down(&mut session, &mut t, (moved.0 + 150, moved.1 + 15));
    drag(&mut session, &mut c, &mut t, (moved.0 + 150, moved.1 + 15), (60, 60));
    assert_eq!(c.origin_at(&mut session, &mut t, (moved.0 + 200, moved.1 + 100)), moved, "a later touch moved the window");
}

#[test]
fn an_app_resizes_its_window_by_touch() {
    let Some(mut session) = session() else { return };
    let mut c = Client::connect(&session);
    let (mut t, origin) = window(&mut session, &mut c);

    // The bottom-right corner, in the shadow outside the window geometry
    // (GTK's resize border): it grows, its top-left corner stays.
    let at = (origin.0 + START.0 + 6, origin.1 + START.1 + 6);
    let serial = c.touch_down(&mut session, &mut t, at);
    t.toplevel.resize(&c.seat, serial, xdg_toplevel::ResizeEdge::BottomRight);
    c.settle(&mut t);
    assert!(c.state.configure.is_some_and(|(_, r)| r), "resizing: {:?}", c.state.configure);
    drag(&mut session, &mut c, &mut t, at, (60, 40));
    let grown = (START.0 + 60, START.1 + 40);
    assert_eq!(t.size, grown, "the bottom-right corner followed the finger\n{}", tail(&session));
    assert!(c.state.configure.is_some_and(|(_, r)| !r), "no longer resizing once lifted: {:?}", c.state.configure);
    let o = c.origin_at(&mut session, &mut t, (origin.0 + 100, origin.1 + 100));
    assert_eq!(o, origin, "the top-left corner stayed");

    // The top-left corner: it shrinks, its bottom-right corner stays.
    let at = (origin.0 - 6, origin.1 - 6);
    let serial = c.touch_down(&mut session, &mut t, at);
    t.toplevel.resize(&c.seat, serial, xdg_toplevel::ResizeEdge::TopLeft);
    c.settle(&mut t);
    drag(&mut session, &mut c, &mut t, at, (30, 20));
    assert_eq!(t.size, (grown.0 - 30, grown.1 - 20), "the top-left corner followed the finger\n{}", tail(&session));
    let o = c.origin_at(&mut session, &mut t, (origin.0 + 100, origin.1 + 100));
    assert_eq!(o, (origin.0 + 30, origin.1 + 20), "the bottom-right corner stayed where it was\n{}", tail(&session));
    assert!(c.state.configure.is_some_and(|(_, r)| !r), "no longer resizing once lifted: {:?}", c.state.configure);
}

/// The same by the pointer, pressed in the shadow outside the window
/// geometry (GTK's resize border): the press reaches the app, and its
/// resize request follows the pointer.
#[test]
fn an_app_resizes_its_window_from_its_border_by_pointer() {
    let Some(mut session) = session() else { return };
    let mut c = Client::connect(&session);
    let (mut t, origin) = window(&mut session, &mut c);
    let at = (origin.0 - 6, origin.1 + 100);
    session.input(&format!("move {} {}", at.0, at.1));
    c.settle(&mut t);
    assert!(c.state.pointer.as_ref().is_some_and(|(s, ..)| s == &t.surface), "the pointer in the border is on the window");
    c.state.press = None;
    session.input("button 272 1");
    c.settle(&mut t);
    let serial = c.state.press.expect("the press in the border reached the app");
    t.toplevel.resize(&c.seat, serial, xdg_toplevel::ResizeEdge::Left);
    c.settle(&mut t);
    for step in 1..=10 {
        session.input(&format!("move {} {}", at.0 - step * 5, at.1));
        c.settle(&mut t);
    }
    session.input("button 272 0");
    c.settle(&mut t);
    c.settle(&mut t);
    assert_eq!(t.size, (START.0 + 50, START.1), "the left edge followed the pointer");
    let o = c.origin_at(&mut session, &mut t, (origin.0 + 100, origin.1 + 100));
    assert_eq!(o, (origin.0 - 50, origin.1), "the right edge stayed");
}

/// A touch serial that is not the requesting client's, or of a touch that
/// has lifted, starts nothing: the touch goes on to its own app.
#[test]
fn a_touch_serial_of_another_app_or_an_old_one_is_refused() {
    let Some(mut session) = session() else { return };
    // B, then A over it (new windows open in the middle).
    let mut b = Client::connect(&session);
    let (mut tb, origin_b) = window(&mut session, &mut b);
    let mut a = Client::connect(&session);
    let (mut ta, origin_a) = window(&mut session, &mut a);
    let on_a = (origin_a.0 + 150, origin_a.1 + 15);
    let probe_a = (origin_a.0 + 100, origin_a.1 + 100);

    // A's touch serial, used by B.
    let serial = a.touch_down(&mut session, &mut ta, on_a);
    tb.toplevel._move(&b.seat, serial);
    b.settle(&mut tb);
    tb.toplevel.resize(&b.seat, serial, xdg_toplevel::ResizeEdge::BottomRight);
    b.settle(&mut tb);
    a.state.motions = 0;
    drag(&mut session, &mut a, &mut ta, on_a, (40, 30));
    b.settle(&mut tb);
    assert!(a.state.motions >= 10, "the touch went on to its own app ({} motions)", a.state.motions);
    assert_eq!(tb.size, START, "the other app's window was not resized");
    assert_eq!(a.origin_at(&mut session, &mut ta, probe_a), origin_a, "the touched window did not move");

    // A's own serial, once that touch has lifted, as a new one is down.
    let new = a.touch_down(&mut session, &mut ta, on_a);
    assert_ne!(new, serial);
    ta.toplevel._move(&a.seat, serial);
    a.settle(&mut ta);
    a.state.motions = 0;
    drag(&mut session, &mut a, &mut ta, on_a, (40, 30));
    assert!(a.state.motions >= 10, "the touch went on to the app ({} motions)", a.state.motions);
    assert_eq!(a.origin_at(&mut session, &mut ta, probe_a), origin_a, "an old touch serial moved the window");

    // B, under A, did not move either.
    ta.toplevel.destroy();
    ta.xdg.destroy();
    ta.surface.destroy();
    a.queue.roundtrip(&mut a.state).unwrap();
    let probe_b = (origin_b.0 + 100, origin_b.1 + 100);
    assert_eq!(b.origin_at(&mut session, &mut tb, probe_b), origin_b, "the other app's window did not move");
}
